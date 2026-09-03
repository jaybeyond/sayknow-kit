import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"

type Props = {
  open: boolean
  message: string
  confirmLabel: string
  cancelLabel: string
  destructive?: boolean
  onConfirm: () => void
  onCancel: () => void
}

/**
 * In-app confirmation. The macOS webview swallows `window.confirm`, returning
 * false without ever showing a dialog, so every destructive action must ask
 * here instead of through the browser primitive.
 */
export function ConfirmDialog(props: Props) {
  return (
    <Dialog open={props.open} onOpenChange={(open) => { if (!open) props.onCancel() }}>
      <DialogContent className="w-[320px] gap-3 p-4" showCloseButton={false}>
        <DialogHeader>
          <DialogTitle className="text-sm">{props.message}</DialogTitle>
          <DialogDescription className="sr-only">{props.message}</DialogDescription>
        </DialogHeader>
        <DialogFooter className="flex-row justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={props.onCancel}>
            {props.cancelLabel}
          </Button>
          <Button
            variant={props.destructive ? "destructive" : "default"}
            size="sm"
            onClick={props.onConfirm}
          >
            {props.confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
