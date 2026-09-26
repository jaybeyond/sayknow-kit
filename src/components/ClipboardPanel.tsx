import { useEffect, useMemo, useRef, useState } from "react"
import {
  Check,
  Clipboard as ClipboardIcon,
  Languages as TranslateIcon,
  NotebookPen,
  Pause,
  Pencil,
  Pin,
  PinOff,
  Play,
  Search,
  StickyNote,
  Trash2,
  X,
} from "lucide-react"
import { ConfirmDialog } from "@/components/ConfirmDialog"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Input } from "@/components/ui/input"
import { Separator } from "@/components/ui/separator"
import { Textarea } from "@/components/ui/textarea"
import { useClipboardHistory } from "@/hooks/useClipboardHistory"
import type { Settings } from "@/hooks/useSettings"
import { useT } from "@/i18n"
import { isMemo } from "@/lib/clipboard-history"
import { timeAgo } from "@/lib/history"
import { cn } from "@/lib/utils"

type Props = {
  settings: Settings
  /** Optional integration: clicking the "send to translate" button. */
  onSendToTranslate?: (text: string) => void
}

type KindFilter = "all" | "clip" | "memo"

type Translate = (key: string) => string

/** Pseudo-id for the "new memo" composer so it shares the single-editor slot. */
const NEW_MEMO = "__new_memo__"

export function ClipboardPanel({ settings, onSendToTranslate }: Props) {
  const { t } = useT(settings.uiLocale)
  const {
    entries,
    loaded,
    captureEnabled,
    setCaptureEnabled,
    reuse,
    remove,
    togglePin,
    setNote,
    createMemo,
    updateMemo,
    clear,
    wipe,
  } = useClipboardHistory()
  const [q, setQ] = useState("")
  const [kindFilter, setKindFilter] = useState<KindFilter>("all")
  const [copiedId, setCopiedId] = useState<string | null>(null)
  const [editingNoteId, setEditingNoteId] = useState<string | null>(null)
  const [noteDraft, setNoteDraft] = useState("")
  // Only one memo editor is open at a time: NEW_MEMO for the composer, or the
  // id of the memo being rewritten.
  const [memoEditorId, setMemoEditorId] = useState<string | null>(null)
  const [pendingClear, setPendingClear] = useState<"unpinned" | "all" | null>(null)
  const [pendingMemoDelete, setPendingMemoDelete] = useState<string | null>(null)
  const noteTextareaRef = useRef<HTMLTextAreaElement>(null)
  const listRef = useRef<HTMLDivElement>(null)

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase()
    return entries.filter((e) => {
      if (kindFilter === "memo" && !isMemo(e)) return false
      if (kindFilter === "clip" && isMemo(e)) return false
      if (!needle) return true
      return (
        e.text.toLowerCase().includes(needle) ||
        (e.note ?? "").toLowerCase().includes(needle)
      )
    })
  }, [entries, q, kindFilter])

  // Focus the note textarea when it appears so the user can type immediately.
  useEffect(() => {
    if (editingNoteId && noteTextareaRef.current) {
      noteTextareaRef.current.focus()
      const end = noteTextareaRef.current.value.length
      noteTextareaRef.current.setSelectionRange(end, end)
    }
  }, [editingNoteId])

  function openNoteEditor(id: string, existing: string | null | undefined) {
    setMemoEditorId(null)
    setEditingNoteId(id)
    setNoteDraft(existing ?? "")
  }

  function cancelNoteEdit() {
    setEditingNoteId(null)
    setNoteDraft("")
  }

  async function commitNote(id: string) {
    await setNote(id, noteDraft)
    cancelNoteEdit()
  }

  function openMemoEditor(id: string) {
    cancelNoteEdit()
    setMemoEditorId(id)
  }

  function startNewMemo() {
    openMemoEditor(NEW_MEMO)
    // A memo saved while only clips are shown would vanish from view.
    if (kindFilter === "clip") setKindFilter("all")
    if (listRef.current) listRef.current.scrollTop = 0
  }

  async function commitMemo(text: string) {
    const id = memoEditorId
    if (!id) return
    const saved = id === NEW_MEMO ? await createMemo(text) : await updateMemo(id, text)
    if (saved) setMemoEditorId(null)
  }

  async function handleReuse(id: string, text: string) {
    await reuse(text)
    setCopiedId(id)
    window.setTimeout(() => setCopiedId((c) => (c === id ? null : c)), 1200)
  }

  const composing = memoEditorId === NEW_MEMO

  return (
    <div className="flex h-full flex-col">
      {/* Toolbar */}
      <div className="flex items-center gap-1.5 border-b bg-muted/30 px-2.5 py-1.5">
        <div className="relative flex-1">
          <Search className="absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={t("clipboard.search")}
            className="h-7 pl-6 text-xs"
          />
        </div>
        <Button
          variant="ghost"
          size="icon"
          className={cn("h-7 w-7", composing && "bg-amber-500/15 text-amber-700 dark:text-amber-300")}
          onClick={() => (composing ? setMemoEditorId(null) : startNewMemo())}
          title={t("clipboard.newMemo")}
          aria-label={t("clipboard.newMemo")}
          aria-pressed={composing}
        >
          <NotebookPen className="h-3.5 w-3.5" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          onClick={() => void setCaptureEnabled(!captureEnabled)}
          title={
            captureEnabled
              ? t("clipboard.pause")
              : t("clipboard.resume")
          }
          aria-label={
            captureEnabled
              ? t("clipboard.pause")
              : t("clipboard.resume")
          }
        >
          {captureEnabled ? (
            <Pause className="h-3.5 w-3.5" />
          ) : (
            <Play className="h-3.5 w-3.5 text-amber-500" />
          )}
        </Button>
        {entries.length > 0 && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-[10px] text-muted-foreground hover:text-destructive"
                title={t("clipboard.clearMenuTooltip")}
                aria-label={t("clipboard.clearMenuTooltip")}
              >
                <Trash2 className="h-3 w-3" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-56">
              <DropdownMenuItem onSelect={() => setPendingClear("unpinned")}>
                {t("clipboard.clearUnpinned")}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-destructive focus:text-destructive"
                onSelect={() => setPendingClear("all")}
              >
                {t("clipboard.clearAll")}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </div>

      {/* Kind filter */}
      <div
        role="radiogroup"
        aria-label={t("clipboard.filterLabel")}
        className="flex items-center gap-0.5 border-b px-2.5 py-1"
      >
        {(
          [
            ["all", t("clipboard.filterAll")],
            ["clip", t("clipboard.filterClips")],
            ["memo", t("clipboard.filterMemos")],
          ] as const
        ).map(([value, label]) => (
          <button
            key={value}
            type="button"
            role="radio"
            aria-checked={kindFilter === value}
            onClick={() => setKindFilter(value)}
            className={cn(
              "rounded px-2 py-0.5 text-[10px] transition-colors duration-150",
              kindFilter === value
                ? "bg-foreground/10 font-medium text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {label}
          </button>
        ))}
      </div>

      {!captureEnabled && (
        <div className="border-b bg-amber-500/10 px-3 py-1.5 text-[10px] text-amber-700 dark:text-amber-400">
          {t("clipboard.pausedNotice")}
        </div>
      )}

      {/* List */}
      <div ref={listRef} className="flex-1 overflow-y-auto">
        {composing && (
          <div className="border-b border-l-2 border-l-amber-400 bg-amber-50 px-3 py-2 dark:bg-amber-400/[0.07]">
            <MemoEditor
              initial=""
              t={t}
              onCancel={() => setMemoEditorId(null)}
              onSave={commitMemo}
            />
          </div>
        )}
        {!loaded ? (
          <EmptyState text={t("clipboard.loading")} />
        ) : filtered.length === 0 ? (
          composing ? null : q ? (
            <EmptyState
              icon={<ClipboardIcon className="h-8 w-8 opacity-30" />}
              text={t("clipboard.noResult")}
            />
          ) : kindFilter === "memo" ? (
            <EmptyState
              icon={<StickyNote className="h-8 w-8 opacity-30" />}
              text={t("clipboard.memoEmpty")}
              hint={t("clipboard.memoEmptyHint")}
            />
          ) : (
            <EmptyState
              icon={<ClipboardIcon className="h-8 w-8 opacity-30" />}
              text={t("clipboard.empty")}
              hint={t("clipboard.emptyHint")}
            />
          )
        ) : (
          filtered.map((e) => {
            const memo = isMemo(e)
            if (memo && memoEditorId === e.id) {
              return (
                <div
                  key={e.id}
                  className="border-b border-l-2 border-l-amber-400 bg-amber-50 px-3 py-2 last:border-b-0 dark:bg-amber-400/[0.07]"
                >
                  <MemoEditor
                    initial={e.text}
                    t={t}
                    onCancel={() => setMemoEditorId(null)}
                    onSave={commitMemo}
                  />
                </div>
              )
            }
            const isEditingNote = !memo && editingNoteId === e.id
            const hasNote = !memo && !!e.note && e.note.trim().length > 0
            return (
            <div
              key={e.id}
              data-kind={memo ? "memo" : "clip"}
              className={cn(
                "group relative border-b text-left text-xs last:border-b-0",
                memo
                  ? "border-l-2 border-l-amber-400 bg-amber-50 hover:bg-amber-100/70 dark:bg-amber-400/[0.07] dark:hover:bg-amber-400/[0.12]"
                  : "hover:bg-accent/40",
                !memo && e.pinned && "bg-accent/20",
                hasNote && "bg-amber-500/5",
              )}
            >
              <button
                type="button"
                onClick={() => void handleReuse(e.id, e.text)}
                className="flex w-full flex-col gap-0.5 px-3 py-2 pr-20 text-left"
                title={t("clipboard.reuseTooltip")}
              >
                <div
                  className={cn(
                    "whitespace-pre-wrap text-foreground",
                    memo ? "line-clamp-4" : "line-clamp-2",
                  )}
                >
                  {memo ? e.text : e.preview}
                </div>
                {hasNote && !isEditingNote && (
                  <div className="mt-1 flex items-start gap-1 rounded-sm border-l-2 border-amber-500/60 bg-amber-500/5 px-1.5 py-1 text-[10px] italic text-amber-900 dark:text-amber-200">
                    <StickyNote className="mt-px h-2.5 w-2.5 shrink-0 text-amber-600/80" />
                    <span className="whitespace-pre-wrap">{e.note}</span>
                  </div>
                )}
                <div className="mt-0.5 flex items-center gap-1.5 text-[10px] text-muted-foreground">
                  {memo && (
                    <span className="inline-flex items-center gap-0.5 font-medium text-amber-700 dark:text-amber-400">
                      <StickyNote className="h-2.5 w-2.5" />
                      {t("clipboard.memoBadge")}
                    </span>
                  )}
                  {!memo && e.pinned && (
                    <Pin className="h-2.5 w-2.5 text-foreground/70" />
                  )}
                  {memo && <span>·</span>}
                  <span>{timeAgo(e.ts)}</span>
                  <span>·</span>
                  <span>{characterCountLabel(e.text)}</span>
                  {copiedId === e.id && (
                    <>
                      <span>·</span>
                      <span className="inline-flex items-center gap-0.5 text-emerald-600 dark:text-emerald-500">
                        <Check className="h-2.5 w-2.5" />
                        {t("clipboard.copied")}
                      </span>
                    </>
                  )}
                </div>
              </button>
              {isEditingNote && (
                <div
                  className="border-t bg-background px-3 py-2"
                  onClick={(ev) => ev.stopPropagation()}
                >
                  <Textarea
                    ref={noteTextareaRef}
                    value={noteDraft}
                    onChange={(ev) => setNoteDraft(ev.target.value)}
                    placeholder={t("clipboard.notePlaceholder")}
                    rows={3}
                    className="min-h-[60px] resize-y text-xs"
                    onKeyDown={(ev) => {
                      if (ev.key === "Escape") {
                        ev.preventDefault()
                        cancelNoteEdit()
                      } else if (
                        ev.key === "Enter" &&
                        (ev.metaKey || ev.ctrlKey)
                      ) {
                        ev.preventDefault()
                        void commitNote(e.id)
                      }
                    }}
                  />
                  <div className="mt-1.5 flex items-center justify-between text-[10px] text-muted-foreground">
                    <span>{t("clipboard.noteHotkeyHint")}</span>
                    <div className="flex items-center gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-2 text-[10px]"
                        onClick={cancelNoteEdit}
                      >
                        {t("clipboard.noteCancel")}
                      </Button>
                      <Button
                        size="sm"
                        className="h-6 px-2 text-[10px]"
                        onClick={() => void commitNote(e.id)}
                      >
                        {t("clipboard.noteSave")}
                      </Button>
                    </div>
                  </div>
                </div>
              )}
              <div
                className={cn(
                  "absolute right-1.5 top-1.5 flex items-center gap-0.5 transition-opacity",
                  hasNote || isEditingNote
                    ? "opacity-100"
                    : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100",
                )}
              >
                {onSendToTranslate && (
                  <button
                    type="button"
                    onClick={(ev) => {
                      ev.stopPropagation()
                      onSendToTranslate(e.text)
                    }}
                    className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
                    aria-label={t("clipboard.sendToTranslate")}
                    title={t("clipboard.sendToTranslate")}
                  >
                    <TranslateIcon className="h-3 w-3" />
                  </button>
                )}
                {memo ? (
                  <button
                    type="button"
                    onClick={(ev) => {
                      ev.stopPropagation()
                      openMemoEditor(e.id)
                    }}
                    className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
                    aria-label={t("clipboard.editMemo")}
                    title={t("clipboard.editMemo")}
                  >
                    <Pencil className="h-3 w-3" />
                  </button>
                ) : (
                  <>
                    <button
                      type="button"
                      onClick={(ev) => {
                        ev.stopPropagation()
                        if (isEditingNote) {
                          cancelNoteEdit()
                        } else {
                          openNoteEditor(e.id, e.note ?? null)
                        }
                      }}
                      className={cn(
                        "rounded p-1 hover:bg-background",
                        hasNote
                          ? "text-amber-600 hover:text-amber-700 dark:text-amber-400 dark:hover:text-amber-300"
                          : "text-muted-foreground hover:text-foreground",
                      )}
                      aria-label={
                        hasNote ? t("clipboard.editNote") : t("clipboard.addNote")
                      }
                      title={
                        hasNote ? t("clipboard.editNote") : t("clipboard.addNote")
                      }
                    >
                      <StickyNote className="h-3 w-3" />
                    </button>
                    <button
                      type="button"
                      onClick={(ev) => {
                        ev.stopPropagation()
                        void togglePin(e.id)
                      }}
                      className="rounded p-1 text-muted-foreground hover:bg-background hover:text-foreground"
                      aria-label={
                        e.pinned ? t("clipboard.unpin") : t("clipboard.pin")
                      }
                      title={e.pinned ? t("clipboard.unpin") : t("clipboard.pin")}
                    >
                      {e.pinned ? (
                        <PinOff className="h-3 w-3" />
                      ) : (
                        <Pin className="h-3 w-3" />
                      )}
                    </button>
                  </>
                )}
                <button
                  type="button"
                  onClick={(ev) => {
                    ev.stopPropagation()
                    // Memos are the user's own writing, so deleting one asks
                    // first; clips can be re-copied from their source.
                    if (memo) setPendingMemoDelete(e.id)
                    else void remove(e.id)
                  }}
                  className="rounded p-1 text-muted-foreground hover:bg-background hover:text-destructive"
                  aria-label={t("clipboard.delete")}
                  title={t("clipboard.delete")}
                >
                  <X className="h-3 w-3" />
                </button>
              </div>
            </div>
            )
          })
        )}
      </div>

      {/* Footer hint */}
      {loaded && entries.length > 0 && (
        <>
          <Separator />
          <div className="px-3 py-1 text-[10px] text-muted-foreground">
            {t("clipboard.footerHint")}
          </div>
        </>
      )}

      <ConfirmDialog
        open={pendingClear !== null}
        message={pendingClear === "all" ? t("clipboard.confirmWipe") : t("clipboard.confirmClear")}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        onCancel={() => setPendingClear(null)}
        onConfirm={() => {
          const action = pendingClear
          setPendingClear(null)
          if (action === "all") void wipe()
          else if (action === "unpinned") void clear()
        }}
      />
      <ConfirmDialog
        open={pendingMemoDelete !== null}
        message={t("clipboard.confirmDeleteMemo")}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        onCancel={() => setPendingMemoDelete(null)}
        onConfirm={() => {
          const id = pendingMemoDelete
          setPendingMemoDelete(null)
          if (id) void remove(id)
        }}
      />
    </div>
  )
}

/** Sticky-note editor shared by the composer and in-place memo edits. */
function MemoEditor({
  initial,
  t,
  onCancel,
  onSave,
}: {
  initial: string
  t: Translate
  onCancel: () => void
  onSave: (text: string) => Promise<void>
}) {
  const [draft, setDraft] = useState(initial)
  const [saving, setSaving] = useState(false)
  const ref = useRef<HTMLTextAreaElement>(null)
  const blank = draft.trim().length === 0

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.focus()
    el.setSelectionRange(el.value.length, el.value.length)
  }, [])

  async function save() {
    if (blank || saving) return
    setSaving(true)
    try {
      await onSave(draft)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div>
      <Textarea
        ref={ref}
        value={draft}
        onChange={(ev) => setDraft(ev.target.value)}
        placeholder={t("clipboard.memoPlaceholder")}
        aria-label={t("clipboard.memoPlaceholder")}
        rows={4}
        className="min-h-[76px] resize-y border-amber-300/70 bg-background/80 text-xs focus-visible:ring-amber-400/60 dark:border-amber-400/30"
        onKeyDown={(ev) => {
          if (ev.key === "Escape") {
            ev.preventDefault()
            onCancel()
          } else if (ev.key === "Enter" && (ev.metaKey || ev.ctrlKey)) {
            ev.preventDefault()
            void save()
          }
        }}
      />
      <div className="mt-1.5 flex items-center justify-between text-[10px] text-muted-foreground">
        <span>{t("clipboard.noteHotkeyHint")}</span>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-[10px]"
            onClick={onCancel}
          >
            {t("clipboard.noteCancel")}
          </Button>
          <Button
            size="sm"
            className="h-6 px-2 text-[10px]"
            disabled={blank || saving}
            onClick={() => void save()}
          >
            {t("clipboard.noteSave")}
          </Button>
        </div>
      </div>
    </div>
  )
}

function EmptyState({
  icon,
  text,
  hint,
}: {
  icon?: React.ReactNode
  text: string
  hint?: string
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-6 py-12 text-center text-[11px] text-muted-foreground">
      {icon}
      <div>{text}</div>
      {hint && <div className="text-[10px] opacity-70">{hint}</div>}
    </div>
  )
}

function characterCountLabel(text: string): string {
  const n = text.length
  if (n < 1000) return `${n}자`
  return `${(n / 1000).toFixed(1)}k자`
}
