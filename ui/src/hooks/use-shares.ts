import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { canTransfer } from "@/lib/access";
import { useAuthStore } from "@/stores/auth-store";
import {
  ownerApi,
  sharesApi,
  type DefShare,
  type ShareKind,
} from "@/lib/api/shares";

export function useDefShares(
  kind: ShareKind,
  id: string | undefined,
  owner?: string | null,
) {
  return useQuery({
    queryKey: ["def-shares", kind, id, owner ?? null],
    queryFn: () => sharesApi.get(kind, id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useSetDefShares(kind: ShareKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      shares,
      owner,
    }: {
      id: string;
      shares: DefShare[];
      owner?: string | null;
    }) => sharesApi.set(kind, id, shares, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["def-shares", kind, vars.id] });
    },
  });
}

/**
 * Hand an owned resource to another member. On success both the grantee set
 * and the listing are stale: the row's identity has changed, so anything
 * cached against the old owner now addresses nothing.
 */
export function useTransferOwner(kind: ShareKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      owner,
      from,
      keepManage,
    }: {
      id: string;
      owner: string;
      from?: string | null;
      /** Leave the outgoing owner a `manage` grant. The dialog always states
       *  it, so the wire format never depends on a default. */
      keepManage: boolean;
    }) => ownerApi.set(kind, id, owner, from, keepManage),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["def-shares", kind, vars.id] });
      // Every listing is cached under its own path prefix, the skills
      // included, so the kind is the key.
      qc.invalidateQueries({ queryKey: [kind] });
    },
  });
}

/**
 * [`canTransfer`] asked of the signed-in user: a predicate rather than an
 * answer, so a list can ask it per row.
 */
export function useTransferStanding(): (owner?: string | null) => boolean {
  const identity = useAuthStore((s) => s.identity);
  return (owner) => canTransfer(identity, owner);
}
