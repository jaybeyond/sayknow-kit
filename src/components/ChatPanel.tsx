import { useEffect, useRef, useState } from "react"
import {
  ArrowUp,
  Check,
  Copy,
  Loader2,
  MessageSquare,
  MessagesSquare,
  Pencil,
  Plus,
  RotateCcw,
  Settings as SettingsIcon,
  Square,
  Trash2,
  X,
} from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import { Button } from "@/components/ui/button"
import { Textarea } from "@/components/ui/textarea"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Separator } from "@/components/ui/separator"
import { useChat } from "@/hooks/useChat"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { isTauri } from "@/lib/runtime"
import { timeAgo, type Conversation } from "@/lib/chat-history"
import { cn } from "@/lib/utils"

type Props = {
  settings: Settings
  update: (patch: Partial<Settings>) => void
}

export function ChatPanel({ settings, update }: Props) {
  const { t } = useT(settings.uiLocale)
  const {
    list,
    current,
    messages,
    sending,
    error,
    setError,
    send,
    stop,
    regenerate,
    editAndResend,
    newConversation,
    switchTo,
    deleteConversation,
  } = useChat({
    apiKey: settings.apiKey,
    baseURL: settings.baseURL,
    model: settings.model,
    fallbackModel: settings.fallbackModel,
  })
  const [draft, setDraft] = useState("")
  const [editingId, setEditingId] = useState<string | null>(null)
  const [copiedId, setCopiedId] = useState<string | null>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  useEffect(() => {
    const el = scrollRef.current
    if (!el) return
    el.scrollTop = el.scrollHeight
  }, [messages, sending])

  // Reset edit state when switching conversation. Adjusted during render
  // rather than in an effect: React re-runs this component immediately with
  // the corrected state instead of committing the stale draft first and then
  // cascading a second render.
  const [lastConversationId, setLastConversationId] = useState(current?.id)
  if (current?.id !== lastConversationId) {
    setLastConversationId(current?.id)
    setEditingId(null)
    setDraft("")
    setError(null)
  }

  function submit() {
    const text = draft.trim()
    if (!text || sending) return
    if (editingId) {
      const id = editingId
      setEditingId(null)
      setDraft("")
      void editAndResend(id, text)
      return
    }
    setDraft("")
    void send(text)
  }

  function startEdit(msgId: string, content: string) {
    setEditingId(msgId)
    setDraft(content)
    setTimeout(() => textareaRef.current?.focus(), 0)
  }

  function cancelEdit() {
    setEditingId(null)
    setDraft("")
  }

  async function copy(text: string, id: string) {
    try {
      await navigator.clipboard.writeText(text)
      setCopiedId(id)
      window.setTimeout(() => setCopiedId((c) => (c === id ? null : c)), 1200)
    } catch {
      /* clipboard blocked */
    }
  }

  function handleDeleteCurrent() {
    if (!current) return
    if (confirm(t("chat.confirmDelete"))) deleteConversation(current.id)
  }

  return (
    <div className="flex h-full flex-col">
      {/* Top bar */}
      <div
        className="flex items-center gap-1 border-b bg-muted/30 px-2 py-1.5"
        data-tauri-drag-region
      >
        <ConversationsMenu
          uiLocale={settings.uiLocale}
          list={list}
          currentId={current?.id ?? null}
          onSwitch={switchTo}
          onNew={newConversation}
          onDelete={(id) => {
            if (confirm(t("chat.confirmDelete"))) deleteConversation(id)
          }}
        />
        <button
          type="button"
          onClick={newConversation}
          className="ml-1 inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:bg-background hover:text-foreground"
          title={t("chat.new")}
        >
          <Plus className="h-3 w-3" />
          <span className="hidden xs:inline">{t("chat.new")}</span>
        </button>
        <div className="ml-2 flex-1 truncate text-xs text-muted-foreground">
          {current?.title}
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          onClick={handleDeleteCurrent}
          disabled={!current || messages.length === 0}
          aria-label={t("chat.confirmDelete")}
          title={t("chat.confirmDelete")}
        >
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
        <QuickMenu settings={settings} update={update} />
      </div>

      {/* Messages */}
      <div ref={scrollRef} className="flex-1 overflow-y-auto px-3 py-3">
        {messages.length === 0 ? (
          <div className="flex h-full flex-col items-center justify-center px-4 text-center">
            <div className="mb-2 flex h-9 w-9 items-center justify-center rounded-xl bg-muted">
              <MessageSquare className="h-4 w-4 text-muted-foreground" />
            </div>
            <div className="text-sm font-medium">{t("chat.empty.title")}</div>
            <p className="mt-1 max-w-[260px] text-[11px] leading-relaxed text-muted-foreground">
              {t("chat.empty.body")}
            </p>
          </div>
        ) : (
          <div className="space-y-3">
            {messages.map((m) => {
              const isUser = m.role === "user"
              const isEditing = editingId === m.id
              const isLastAssistant =
                !isUser && messages[messages.length - 1]?.id === m.id
              return (
                <div
                  key={m.id}
                  className={cn(
                    "group flex flex-col gap-1",
                    isUser ? "items-end" : "items-start",
                  )}
                >
                  <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
                    {isUser ? t("chat.you") : t("chat.assistant")}
                  </div>
                  <div
                    className={cn(
                      "max-w-[85%] rounded-2xl px-3 py-2 text-[13px] leading-relaxed whitespace-pre-wrap",
                      isUser
                        ? "bg-foreground text-background"
                        : "bg-muted",
                      isEditing && "ring-2 ring-amber-500/40",
                    )}
                  >
                    {m.content}
                  </div>
                  <div
                    className={cn(
                      "flex items-center gap-0.5 opacity-0 transition group-hover:opacity-100",
                      isUser ? "flex-row-reverse" : "flex-row",
                    )}
                  >
                    <MsgActionButton
                      label={copiedId === m.id ? t("copied") : t("copy")}
                      onClick={() => copy(m.content, m.id)}
                    >
                      {copiedId === m.id ? (
                        <Check className="h-3 w-3 text-emerald-500" />
                      ) : (
                        <Copy className="h-3 w-3" />
                      )}
                    </MsgActionButton>
                    {isUser ? (
                      <MsgActionButton
                        label={t("chat.edit")}
                        onClick={() => startEdit(m.id, m.content)}
                        disabled={sending}
                      >
                        <Pencil className="h-3 w-3" />
                      </MsgActionButton>
                    ) : (
                      isLastAssistant && (
                        <MsgActionButton
                          label={t("chat.regenerate")}
                          onClick={() => void regenerate(m.id)}
                          disabled={sending}
                        >
                          <RotateCcw className="h-3 w-3" />
                        </MsgActionButton>
                      )
                    )}
                  </div>
                </div>
              )
            })}
            {sending && (
              <div className="flex flex-col items-start gap-1">
                <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
                  {t("chat.assistant")}
                </div>
                <div className="inline-flex items-center gap-1.5 rounded-2xl bg-muted px-3 py-2 text-[13px] text-muted-foreground">
                  <Loader2 className="h-3 w-3 animate-spin" />
                  {t("chat.thinking")}
                </div>
              </div>
            )}
          </div>
        )}
      </div>

      {error && (
        <div className="flex items-start gap-1.5 border-t bg-destructive/10 px-3 py-1.5 text-[11px] text-destructive">
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={() => setError(null)}
            aria-label={t("common.close")}
            className="shrink-0 hover:opacity-80"
          >
            <X className="h-3 w-3" />
          </button>
        </div>
      )}

      <div className="border-t bg-muted/20 p-2">
        {editingId && (
          <div className="mb-1.5 flex items-center justify-between rounded-md bg-amber-500/15 px-2 py-1 text-[10px] text-amber-700 dark:text-amber-400">
            <span>
              <Pencil className="mr-1 inline h-2.5 w-2.5" />
              {t("chat.editing.title")}
            </span>
            <button type="button" onClick={cancelEdit} className="hover:opacity-80">
              {t("chat.editing.cancel")}
            </button>
          </div>
        )}
        <div className="flex items-end gap-1.5">
          <Textarea
            ref={textareaRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && editingId) {
                e.preventDefault()
                cancelEdit()
                return
              }
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault()
                submit()
              }
            }}
            placeholder={editingId ? t("chat.editing.title") : t("chat.placeholder")}
            className="min-h-[40px] max-h-[140px] flex-1 resize-none text-sm"
            autoFocus
          />
          {sending ? (
            <Button
              size="icon"
              variant="outline"
              onClick={stop}
              className="h-9 w-9 shrink-0 border-destructive/50 bg-destructive/5 text-destructive hover:bg-destructive/15 hover:text-destructive"
              aria-label={t("stop")}
              title={t("stop")}
            >
              <Square className="h-3 w-3 fill-current" />
            </Button>
          ) : (
            <Button
              size="icon"
              onClick={submit}
              disabled={!draft.trim()}
              className="h-9 w-9 shrink-0 rounded-full"
              aria-label={t("chat.send")}
              title={t("chat.send")}
            >
              <ArrowUp className="h-4 w-4" />
            </Button>
          )}
        </div>
      </div>
    </div>
  )
}

function MsgActionButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string
  onClick: () => void
  disabled?: boolean
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
      className="rounded p-1 text-muted-foreground transition hover:bg-background hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
    >
      {children}
    </button>
  )
}

function ConversationsMenu({
  uiLocale,
  list,
  currentId,
  onSwitch,
  onNew,
  onDelete,
}: {
  uiLocale: Settings["uiLocale"]
  list: Conversation[]
  currentId: string | null
  onSwitch: (id: string) => void
  onNew: () => void
  onDelete: (id: string) => void
}) {
  const { t } = useT(uiLocale)
  const [open, setOpen] = useState(false)

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          aria-label={t("chat.conversations")}
        >
          <MessagesSquare className="h-3.5 w-3.5" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" sideOffset={6} className="w-[320px] p-0">
        <button
          type="button"
          onClick={() => {
            onNew()
            setOpen(false)
          }}
          className="flex w-full items-center gap-2 border-b px-3 py-2 text-left text-xs hover:bg-accent/40"
        >
          <Plus className="h-3 w-3" />
          {t("chat.new")}
        </button>
        <div className="max-h-[320px] overflow-y-auto">
          {list.length === 0 ? (
            <div className="px-3 py-8 text-center text-[11px] text-muted-foreground">
              {t("chat.empty.list")}
            </div>
          ) : (
            list.map((c) => {
              const lastMsg = c.messages[c.messages.length - 1]
              const isCurrent = c.id === currentId
              return (
                <div
                  key={c.id}
                  className={cn(
                    "group relative border-b last:border-b-0",
                    isCurrent && "bg-accent/30",
                  )}
                >
                  <button
                    type="button"
                    onClick={() => {
                      onSwitch(c.id)
                      setOpen(false)
                    }}
                    className="flex w-full flex-col gap-0.5 px-3 py-2 pr-9 text-left hover:bg-accent/40"
                  >
                    <div className="line-clamp-1 text-xs font-medium">
                      {c.title || t("chat.untitled")}
                    </div>
                    {lastMsg && (
                      <div className="line-clamp-1 text-[10px] text-muted-foreground">
                        {lastMsg.content}
                      </div>
                    )}
                    <div className="text-[10px] text-muted-foreground">
                      {timeAgo(c.updatedAt)} · {c.messages.length}
                    </div>
                  </button>
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation()
                      onDelete(c.id)
                    }}
                    className="absolute right-1.5 top-2 rounded p-1 text-muted-foreground opacity-0 hover:bg-background hover:text-destructive group-hover:opacity-100"
                    aria-label={t("history.delete")}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </div>
              )
            })
          )}
        </div>
      </PopoverContent>
    </Popover>
  )
}

function QuickMenu({
  settings,
  update,
}: {
  settings: Settings
  update: (p: Partial<Settings>) => void
}) {
  const { t } = useT(settings.uiLocale)
  const [open, setOpen] = useState(false)

  async function openSettings() {
    setOpen(false)
    if (isTauri()) {
      try {
        await invoke("open_settings")
      } catch (e) {
        console.error("open_settings failed:", e)
      }
    }
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          aria-label={t("header.settings")}
        >
          <SettingsIcon className="h-3.5 w-3.5" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" sideOffset={6} className="w-[260px] p-3">
        <div className="space-y-3">
          <div className="flex items-center justify-between gap-2">
            <Label htmlFor="qm-clipboard-chat" className="text-[11px]">
              {t("settings.clipboard.title")}
            </Label>
            <Switch
              id="qm-clipboard-chat"
              checked={settings.clipboardOnHotkey}
              onCheckedChange={(v) => update({ clipboardOnHotkey: v })}
            />
          </div>
          <Separator />
          <Button
            variant="default"
            size="sm"
            className="w-full text-xs"
            onClick={openSettings}
          >
            <SettingsIcon className="h-3 w-3" />
            {t("settings.openButton")}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  )
}
