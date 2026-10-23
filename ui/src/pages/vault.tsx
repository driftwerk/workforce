import { Key } from "lucide-react";
import { VaultBrowser } from "@/components/vault/vault-browser";

export default function VaultPage() {
  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-3">
        <div className="rounded-full bg-muted p-2">
          <Key className="h-5 w-5 text-muted-foreground" />
        </div>
        <div>
          <h1 className="text-lg font-semibold">Vault</h1>
          <p className="text-sm text-muted-foreground">
            Manage secrets stored in the vault.
          </p>
        </div>
      </div>

      <VaultBrowser />
    </div>
  );
}
