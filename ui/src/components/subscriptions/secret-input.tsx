import { useState } from "react";
import { Eye, EyeOff } from "lucide-react";
import { Input } from "@/components/ui/input";

/** A secret typed in hidden, with a toggle to show it. With
 *  `masksVaultRef`, a `${vault:…}` reference reads as stored in the vault. */
export function SecretInput({
  value,
  onChange,
  placeholder,
  masksVaultRef = false,
  "aria-label": ariaLabel,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  masksVaultRef?: boolean;
  "aria-label"?: string;
}) {
  const [visible, setVisible] = useState(false);
  const vaultRef = masksVaultRef && value.startsWith("${vault:");
  return (
    <div className="relative">
      <Input
        type={visible ? "text" : "password"}
        value={vaultRef ? "" : value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={vaultRef ? "(stored in vault)" : placeholder}
        aria-label={ariaLabel}
        className="pr-10"
      />
      <button
        type="button"
        onClick={() => setVisible(!visible)}
        aria-label={visible ? "Hide value" : "Show value"}
        className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
      >
        {visible ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
      </button>
    </div>
  );
}
