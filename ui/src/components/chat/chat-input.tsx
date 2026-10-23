import { useState, useRef, useCallback, type KeyboardEvent } from "react";
import { Send, Square, Paperclip, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

interface ChatInputProps {
  onSend: (message: string, attachments?: File[]) => void;
  onCancel?: () => void;
  disabled?: boolean;
  isStreaming?: boolean;
  contextTokens?: number | null;
  contextMax?: number | null;
  placeholder?: string;
}

export function ChatInput({
  onSend,
  onCancel,
  disabled,
  isStreaming,
  contextTokens,
  contextMax,
  placeholder = "Type a message...",
}: ChatInputProps) {
  const [value, setValue] = useState("");
  const [files, setFiles] = useState<File[]>([]);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const handleSend = useCallback(() => {
    const trimmed = value.trim();
    if (!trimmed && files.length === 0) return;
    onSend(trimmed, files.length > 0 ? files : undefined);
    setValue("");
    setFiles([]);
    if (textareaRef.current) {
      textareaRef.current.style.height = "auto";
    }
  }, [value, files, onSend]);

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (!isStreaming) handleSend();
    }
  };

  const handleInput = () => {
    const el = textareaRef.current;
    if (el) {
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
    }
  };

  const handleFileSelect = () => {
    fileInputRef.current?.click();
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const selected = Array.from(e.target.files ?? []);
    if (selected.length > 0) {
      setFiles((prev) => [...prev, ...selected]);
    }
    // Reset so the same file can be selected again
    e.target.value = "";
  };

  const removeFile = (index: number) => {
    setFiles((prev) => prev.filter((_, i) => i !== index));
  };

  const hasContext = contextTokens != null && contextMax != null && contextMax > 0;
  const contextPercent = hasContext ? Math.min((contextTokens / contextMax) * 100, 100) : 0;

  return (
    <div className="space-y-2">
      {/* File attachments preview */}
      {files.length > 0 && (
        <div className="flex flex-wrap gap-2 px-1">
          {files.map((f, i) => (
            <div
              key={`${f.name}-${i}`}
              className="flex items-center gap-1.5 text-xs rounded-md border bg-muted/50 px-2.5 py-1.5"
            >
              <Paperclip className="h-3 w-3 text-muted-foreground" />
              <span className="max-w-[150px] truncate">{f.name}</span>
              <button
                onClick={() => removeFile(i)}
                className="text-muted-foreground hover:text-foreground transition-colors"
              >
                <X className="h-3 w-3" />
              </button>
            </div>
          ))}
        </div>
      )}

      {/* Input area */}
      <div className="flex items-end gap-2 border rounded-lg p-2 bg-background focus-within:ring-2 focus-within:ring-ring focus-within:ring-offset-1 transition-shadow">
        {/* Attachment button */}
        <Button
          variant="ghost"
          size="icon"
          className="h-8 w-8 shrink-0 text-muted-foreground hover:text-foreground"
          onClick={handleFileSelect}
          disabled={disabled || isStreaming}
          aria-label="Attach file"
        >
          <Paperclip className="h-4 w-4" />
        </Button>

        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFileChange}
        />

        <textarea
          ref={textareaRef}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={handleKeyDown}
          onInput={handleInput}
          placeholder={placeholder}
          disabled={disabled || isStreaming}
          rows={1}
          className="flex-1 resize-none bg-transparent text-sm outline-none placeholder:text-muted-foreground min-h-[40px] max-h-[200px] py-2 px-1"
        />

        {/* Send or Stop button */}
        {isStreaming ? (
          <Button
            size="icon"
            variant="destructive"
            className="h-8 w-8 shrink-0"
            onClick={onCancel}
            aria-label="Stop generating"
          >
            <Square className="h-3.5 w-3.5" />
          </Button>
        ) : (
          <Button
            size="icon"
            className="h-8 w-8 shrink-0"
            onClick={handleSend}
            disabled={disabled || (!value.trim() && files.length === 0)}
            aria-label="Send message"
          >
            <Send className="h-3.5 w-3.5" />
          </Button>
        )}
      </div>

      {/* Context window indicator + hint */}
      <div className="flex items-center justify-between px-1">
        <span className="text-[10px] text-muted-foreground">
          Shift+Enter for newline
        </span>
        {hasContext && (
          <div className="flex items-center gap-2">
            <div className="w-20 h-1 rounded-full bg-muted overflow-hidden">
              <div
                className={cn(
                  "h-full rounded-full transition-all",
                  contextPercent > 80 ? "bg-red-500" : contextPercent > 50 ? "bg-amber-500" : "bg-green-500",
                )}
                style={{ width: `${contextPercent}%` }}
              />
            </div>
            <span className="text-[10px] text-muted-foreground font-mono">
              {Math.round(contextPercent)}%
            </span>
          </div>
        )}
      </div>
    </div>
  );
}
