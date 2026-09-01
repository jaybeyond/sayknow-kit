import { useEffect, useRef, useState } from "react"
import {
  ArrowLeftRight,
  Check,
  Copy,
  CornerDownLeft,
  Loader2,
  Sparkles,
  X,
} from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { readText as readClipboardText } from "@tauri-apps/plugin-clipboard-manager"
import { Button } from "@/components/ui/button"
import { Textarea } from "@/components/ui/textarea"
import { Separator } from "@/components/ui/separator"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { Label } from "@/components/ui/label"
import { LangPicker } from "./LangPicker"
import { useDebounce } from "@/hooks/useDebounce"
import { useModels } from "@/hooks/useModels"
import { useHistory } from "@/hooks/useHistory"
import { useUsage } from "@/hooks/useUsage"
import type { Settings } from "@/hooks/useSettings"
import type { ThemeMode } from "@/hooks/useTheme"
import { isTauri } from "@/lib/runtime"
import {
  buildRefinePrompt,
  buildTranslatePrompt,
  chat,
  LANGS,
  type LangCode,
} from "@/lib/openrouter"
import { useT } from "@/i18n"
import { deeplSupports, deeplTranslate, DeeplError } from "@/lib/deepl"
import { translationMemory } from "@/lib/translation-memory"

const REFINE_PRESETS = [
  { id: "polite", labelKey: "refine.polite", instruction: "Make it more polite and formal." },
  { id: "casual", labelKey: "refine.casual", instruction: "Make it more casual and friendly." },
  { id: "shorter", labelKey: "refine.shorter", instruction: "Make it shorter and more concise." },
  { id: "business", labelKey: "refine.business", instruction: "Use a professional business email tone." },
  { id: "literal", labelKey: "refine.literal", instruction: "Make it more literal." },
] as const

/** What the tab strip can push into the translate tab: clipboard text, or a
 *  history entry complete with its result and language pair. */
export type TranslateInjection = {
  text: string
  output?: string
  from?: LangCode
  to?: LangCode
  /** Changes on every dispatch so identical payloads still trigger. */
  nonce: number
}

type Props = {
  settings: Settings
  update: (patch: Partial<Settings>) => void
  // Kept for symmetry with login flow; unused inside main popover after settings split.
  onLogout?: () => void
  themeMode?: ThemeMode
  setThemeMode?: (m: ThemeMode) => void
  /**
   * Externally injected text — e.g. user clicked "send to translate" on a
   * clipboard history entry. We use a nonce so re-injecting the same text
   * still triggers the effect.
   */
  injectedInput?: TranslateInjection
}

export function TranslatePanel({ settings, update, injectedInput }: Props) {
  const { t } = useT(settings.uiLocale)
  const [input, setInput] = useState("")
  const [output, setOutput] = useState("")
  const [translating, setTranslating] = useState(false)
  const [refining, setRefining] = useState(false)
  const [copied, setCopied] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [refineText, setRefineText] = useState("")
  const abortRef = useRef<AbortController | null>(null)
  const { models } = useModels(settings.apiKey, settings.baseURL)
  // History is listed and restored from the tab strip; this panel only files
  // new entries into it.
  const { add: addHistory } = useHistory()
  const { record: recordUsage } = useUsage()

  const debounced = useDebounce(input, 1500)
  // Tracks the source text that produced the currently-shown `output`.
  // The auto-translate effect uses this to skip re-translating an input
  // that already has a fresh result (e.g. after restoring from history,
  // refining, or finishing a manual translate).
  const lastTranslatedRef = useRef("")

  useEffect(() => {
    if (!isTauri()) return
    void invoke("set_pinned", { pinned: settings.pinned }).catch(() => {})
  }, [settings.pinned])

  useEffect(() => {
    if (!isTauri()) return
    // Compact = wide & short side-by-side layout, optimized for keep-on-screen.
    // Normal = tall stacked layout, optimized for longer texts.
    const [w, h] =
      settings.windowMode === "compact" ? [720, 240] : [480, 580]
    void invoke("resize_main_window", { width: w, height: h }).catch(() => {})
  }, [settings.windowMode])

  useEffect(() => {
    if (!isTauri()) return
    const unlisten = listen<string>("sayknow:open", async (event) => {
      if (event.payload !== "shortcut" || !settings.clipboardOnHotkey) return
      try {
        const text = (await readClipboardText())?.trim() ?? ""
        if (text && text !== input.trim()) {
          changeInput(text)
        }
      } catch {
        /* clipboard empty or denied */
      }
    })
    return () => {
      void unlisten.then((fn) => fn())
    }
  }, [settings.clipboardOnHotkey, input])

  // Pull in text injected from the clipboard history tab. nonce changes even
  // for identical text so the user can re-send the same entry repeatedly.
  /* eslint-disable react-hooks/exhaustive-deps, react-hooks/set-state-in-effect */
  useEffect(() => {
    if (!injectedInput) return
    const text = injectedInput.text.trim()
    if (!text) return
    abortRef.current?.abort()
    changeInput(text)
    // A history entry arrives with its result attached; marking the input as
    // already-translated stops the debounce from paying for it twice.
    setOutput(injectedInput.output ?? "")
    lastTranslatedRef.current = injectedInput.output ? text : ""
    setError(null)
    setTranslating(false)
    setRefining(false)
    if (injectedInput.from && injectedInput.to) {
      update({ from: injectedInput.from, to: injectedInput.to })
    }
  }, [injectedInput?.nonce])
  /* eslint-enable react-hooks/exhaustive-deps, react-hooks/set-state-in-effect */

  /** DeepL is only in play when it is selected, keyed, and covers the pair. */
  const deeplReady =
    settings.translateEngine === "deepl" &&
    !!settings.deeplKey &&
    deeplSupports(settings.from, settings.to)

  function runTranslate(text: string) {
    const trimmed = text.trim()
    if (trimmed.length < 2) return
    abortRef.current?.abort()
    const ctrl = new AbortController()
    abortRef.current = ctrl

    // Auto-translate re-sends the whole field on every typing pause, so the
    // same sentence gets asked for several times on the way to being finished.
    // Serving repeats from memory is what keeps a per-character quota from
    // being burned three to five times over.
    const engine = deeplReady ? "deepl" : "llm"
    const remembered = translationMemory.get(
      engine,
      settings.from,
      settings.to,
      trimmed,
    )
    if (remembered) {
      setOutput(remembered)
      lastTranslatedRef.current = trimmed
      setError(null)
      return
    }

    if (deeplReady) {
      setTranslating(true)
      setError(null)
      deeplTranslate({
        key: settings.deeplKey,
        text: trimmed,
        from: settings.from,
        to: settings.to,
        formality: settings.deeplFormality,
        signal: ctrl.signal,
      })
        .then((result) => {
          if (ctrl.signal.aborted) return
          setOutput(result.text)
          lastTranslatedRef.current = trimmed
          translationMemory.put(
            "deepl",
            settings.from,
            settings.to,
            trimmed,
            result.text,
          )
          addHistory({
            source: trimmed,
            target: result.text,
            from: settings.from,
            to: settings.to,
            model: "DeepL",
          })
        })
        .catch((e) => {
          if (ctrl.signal.aborted) return
          // Out of quota is not a failure the user should be stuck on — the
          // LLM is still there, so fall through to it instead of erroring.
          if (e instanceof DeeplError && e.quotaExceeded) {
            runWithLlm(trimmed, ctrl)
            return
          }
          setError(e instanceof Error ? e.message : String(e))
          setTranslating(false)
        })
        .finally(() => {
          if (!ctrl.signal.aborted && !translatingViaLlmRef.current) {
            setTranslating(false)
          }
        })
      return
    }

    setTranslating(true)
    setError(null)
    runWithLlm(trimmed, ctrl)
  }

  // Set while a DeepL call has handed off to the LLM, so the DeepL promise's
  // finally() doesn't switch the spinner off mid-fallback.
  const translatingViaLlmRef = useRef(false)

  function runWithLlm(trimmed: string, ctrl: AbortController) {
    translatingViaLlmRef.current = true
    chat({
      apiKey: settings.apiKey,
      baseURL: settings.baseURL,
      model: settings.model,
      fallbackModel: settings.fallbackModel,
      messages: buildTranslatePrompt(
        trimmed,
        settings.from,
        settings.to,
        settings.glossary,
        {
          translate: settings.customTranslatePrompt,
          refine: settings.customRefinePrompt,
        },
      ),
      signal: ctrl.signal,
    })
      .then((result) => {
        if (ctrl.signal.aborted) return
        setOutput(result.content)
        lastTranslatedRef.current = trimmed
        translationMemory.put(
          "llm",
          settings.from,
          settings.to,
          trimmed,
          result.content,
        )
        addHistory({
          source: trimmed,
          target: result.content,
          from: settings.from,
          to: settings.to,
          model: result.model,
        })
        if (result.usage) {
          recordUsage({
            modelId: result.model,
            models,
            promptTokens: result.usage.prompt_tokens ?? 0,
            completionTokens: result.usage.completion_tokens ?? 0,
          })
        }
      })
      .catch((e) => {
        if (ctrl.signal.aborted) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        translatingViaLlmRef.current = false
        if (!ctrl.signal.aborted) setTranslating(false)
      })
  }

  function stopTranslate() {
    abortRef.current?.abort()
    setTranslating(false)
    setRefining(false)
  }

  useEffect(() => {
    if (!settings.autoTranslate) return
    const text = debounced.trim()
    // Clearing on short input is handled by changeInput(), where the change
    // actually originates — an effect would cascade an extra render.
    if (text.length < 2) return
    // Skip if this exact input already has a result (e.g. just restored from
    // history or finished a previous translate).
    if (text === lastTranslatedRef.current) return
    runTranslate(debounced)
    return () => abortRef.current?.abort()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    debounced,
    settings.autoTranslate,
    settings.apiKey,
    settings.model,
    settings.from,
    settings.to,
  ])

  /// Single entry point for input changes: too-short input drops any stale
  /// result and cancels an in-flight call, in the handler rather than an
  /// effect watching the value.
  function changeInput(next: string) {
    setInput(next)
    if (next.trim().length < 2) {
      setOutput("")
      setError(null)
      lastTranslatedRef.current = ""
      abortRef.current?.abort()
    }
  }

  function forceTranslate() {
    runTranslate(input)
  }

  function swap() {
    if (settings.from === "auto") return
    update({ from: settings.to, to: settings.from })
    setInput(output)
    setOutput(input)
  }

  async function handleCopy() {
    if (!output) return
    await navigator.clipboard.writeText(output)
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1200)
  }

  async function refine(instruction: string) {
    if (!output || !input.trim()) return
    abortRef.current?.abort()
    const ctrl = new AbortController()
    abortRef.current = ctrl
    setRefining(true)
    setError(null)
    try {
      const result = await chat({
        apiKey: settings.apiKey,
        baseURL: settings.baseURL,
        model: settings.model,
        fallbackModel: settings.fallbackModel,
        messages: buildRefinePrompt(
          input,
          output,
          settings.to,
          instruction,
          settings.glossary,
          {
            translate: settings.customTranslatePrompt,
            refine: settings.customRefinePrompt,
          },
        ),
        signal: ctrl.signal,
      })
      if (ctrl.signal.aborted) return
      setOutput(result.content)
      addHistory({
        source: input.trim(),
        target: result.content,
        from: settings.from,
        to: settings.to,
        model: result.model,
      })
      if (result.usage) {
        recordUsage({
          modelId: result.model,
          models,
          promptTokens: result.usage.prompt_tokens ?? 0,
          completionTokens: result.usage.completion_tokens ?? 0,
        })
      }
    } catch (e) {
      if (ctrl.signal.aborted) return
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      if (!ctrl.signal.aborted) setRefining(false)
    }
  }

  function handleFreeRefine() {
    const t = refineText.trim()
    if (!t) return
    refine(t)
    setRefineText("")
  }


  const targetLang =
    LANGS.find((l) => l.code === settings.to)?.label ?? settings.to
  const isCompact = settings.windowMode === "compact"

  const targetIndicator =
    translating || refining ? (
      <button
        type="button"
        onClick={stopTranslate}
        className="group inline-flex items-center gap-1 rounded-full border border-border/60 bg-background px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground hover:border-destructive/60 hover:text-destructive"
        aria-label={t("stop")}
      >
        <Loader2 className="h-2.5 w-2.5 animate-spin group-hover:hidden" />
        <X className="hidden h-2.5 w-2.5 group-hover:inline" />
        <span>{targetLang}</span>
        <span className="hidden text-destructive group-hover:inline">{t("stop")}</span>
      </button>
    ) : (
      <span className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {targetLang}
      </span>
    )

  const outputBody = output ? (
    output
  ) : translating ? (
    <span className="text-muted-foreground">{t("output.translating")}</span>
  ) : input.trim().length < 2 ? (
    <span className="text-muted-foreground">
      {settings.autoTranslate
        ? t("output.emptyAuto")
        : t("output.emptyManual")}
    </span>
  ) : !settings.autoTranslate ? (
    <span className="text-muted-foreground">{t("output.waitManual")}</span>
  ) : null

  return (
    <div className="flex h-full flex-col">
      <div
        className="flex items-center gap-1 border-b bg-muted/30 px-2 py-1.5"
        data-tauri-drag-region
      >
        <LangPicker
          value={settings.from}
          onChange={(v) => update({ from: v })}
          showAuto
          uiLocale={settings.uiLocale}
        />
        <Button
          variant="ghost"
          size="icon"
          className="h-6 w-6 shrink-0"
          onClick={swap}
          disabled={settings.from === "auto"}
          aria-label={t("header.swap")}
        >
          <ArrowLeftRight className="h-3 w-3" />
        </Button>
        <LangPicker
          value={settings.to}
          onChange={(v) => update({ to: v })}
          uiLocale={settings.uiLocale}
        />
      </div>

      {isCompact ? (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-1 flex-col px-3 py-2">
            <Textarea
              value={input}
              onChange={(e) => changeInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault()
                  forceTranslate()
                }
              }}
              placeholder={
                settings.autoTranslate
                  ? t("input.placeholderAuto")
                  : t("input.placeholderManual")
              }
              autoFocus
              className="field-sizing-fixed min-h-0 flex-1 resize-none overflow-y-auto rounded-none border-0 bg-transparent p-0 text-[13px] leading-relaxed shadow-none transition-none placeholder:text-muted-foreground/70 focus-visible:border-0 focus-visible:shadow-none focus-visible:ring-0 focus-visible:outline-none dark:bg-transparent"
            />
          </div>
          <Separator orientation="vertical" />
          <div className="flex min-w-0 flex-1 flex-col px-3 py-2">
            <div className="mb-1 flex items-center">{targetIndicator}</div>
            <div
              className="flex-1 overflow-y-auto whitespace-pre-wrap text-[13px] leading-relaxed"
              aria-live="polite"
            >
              {outputBody}
            </div>
          </div>
        </div>
      ) : (
        <>
          <div className="flex min-h-[120px] max-h-[40%] basis-[36%] shrink-0 flex-col overflow-hidden px-3 pt-3">
            <Textarea
              value={input}
              onChange={(e) => changeInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault()
                  forceTranslate()
                }
              }}
              placeholder={
                settings.autoTranslate
                  ? t("input.placeholderAuto")
                  : t("input.placeholderManual")
              }
              autoFocus
              className="field-sizing-fixed min-h-0 flex-1 resize-none overflow-y-auto rounded-none border-0 bg-transparent p-0 text-[14px] leading-relaxed shadow-none transition-none placeholder:text-muted-foreground/70 focus-visible:border-0 focus-visible:shadow-none focus-visible:ring-0 focus-visible:outline-none dark:bg-transparent"
            />
          </div>

          <div className="flex items-center gap-2 px-3 pb-1.5 pt-1">
            <Separator className="flex-1" />
            {targetIndicator}
            <Separator className="flex-1" />
          </div>

          <div className="min-h-[200px] flex-1 overflow-y-auto px-3 pb-3">
            <div
              className="min-h-[120px] whitespace-pre-wrap text-[14px] leading-relaxed"
              aria-live="polite"
            >
              {outputBody}
            </div>
          </div>
        </>
      )}

      <div className="border-t bg-muted/20 px-2 py-1.5">
        <div className="flex flex-wrap items-center gap-1">
          {!settings.autoTranslate && (
            <Button
              size="sm"
              variant="default"
              className="h-7 rounded-full px-3 text-[11px]"
              onClick={forceTranslate}
              disabled={input.trim().length < 2 || translating}
            >
              {translating ? (
                <Loader2 className="h-3 w-3 animate-spin" />
              ) : (
                <CornerDownLeft className="h-3 w-3" />
              )}
              {t("translate")}
            </Button>
          )}
          {REFINE_PRESETS.map((p) => (
            <Button
              key={p.id}
              size="sm"
              variant="ghost"
              className="h-6 rounded-full px-2.5 text-[11px] hover:bg-background"
              disabled={!output || refining}
              onClick={() => refine(p.instruction)}
            >
              {t(p.labelKey)}
            </Button>
          ))}
          <div className="ml-auto flex items-center gap-0.5">
            <Popover>
              <PopoverTrigger asChild>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-7 w-7"
                  disabled={!output || refining}
                  aria-label={t("freePrompt")}
                >
                  <Sparkles className="h-3.5 w-3.5" />
                </Button>
              </PopoverTrigger>
              <PopoverContent
                align="end"
                className="w-72 p-2.5"
                sideOffset={6}
              >
                <Label className="text-[11px]">{t("freePrompt")}</Label>
                <Textarea
                  value={refineText}
                  onChange={(e) => setRefineText(e.target.value)}
                  placeholder={t("freePrompt.placeholder")}
                  className="mt-1.5 min-h-[60px] text-xs"
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                      e.preventDefault()
                      handleFreeRefine()
                    }
                  }}
                />
                <Button
                  size="sm"
                  className="mt-2 w-full text-xs"
                  onClick={handleFreeRefine}
                  disabled={!refineText.trim() || refining}
                >
                  {refining ? (
                    <Loader2 className="h-3 w-3 animate-spin" />
                  ) : (
                    t("freePrompt.apply")
                  )}
                </Button>
              </PopoverContent>
            </Popover>
            <Button
              size="icon"
              variant="ghost"
              className="h-7 w-7"
              onClick={handleCopy}
              disabled={!output}
              aria-label={t("copy")}
            >
              {copied ? (
                <Check className="h-3.5 w-3.5 text-emerald-500" />
              ) : (
                <Copy className="h-3.5 w-3.5" />
              )}
            </Button>
          </div>
        </div>
      </div>

      {error && (
        <div className="flex items-start gap-1.5 border-t bg-destructive/10 px-3 py-1.5 text-[11px] text-destructive">
          <span className="flex-1">{error}</span>
          <button
            onClick={() => setError(null)}
            aria-label={t("common.close")}
            className="shrink-0 hover:opacity-80"
          >
            <X className="h-3 w-3" />
          </button>
        </div>
      )}
    </div>
  )
}
