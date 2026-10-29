import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { pairingApi } from "@/lib/api/pairing";

export function usePendingPairings() {
  return useQuery({ queryKey: ["pairing-pending"], queryFn: pairingApi.listPending, staleTime: 10_000 });
}

export function usePairedDevices() {
  return useQuery({ queryKey: ["pairing-devices"], queryFn: pairingApi.listDevices, staleTime: 15_000 });
}

export function useApprovePairing() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: pairingApi.approve, onSuccess: () => { qc.invalidateQueries({ queryKey: ["pairing-pending"] }); } });
}

export function useRejectPairing() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: pairingApi.reject, onSuccess: () => { qc.invalidateQueries({ queryKey: ["pairing-pending"] }); } });
}

export function useRevokeDevice() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: pairingApi.revokeDevice, onSuccess: () => { qc.invalidateQueries({ queryKey: ["pairing-devices"] }); } });
}

/** Admin: approve a waiting device straight into a chosen person's account. */
export function useApproveInto() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ code, identityId }: { code: string; identityId: string }) =>
      pairingApi.approveInto(code, identityId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["pairing-pending"] });
      qc.invalidateQueries({ queryKey: ["pairing-devices"] });
    },
  });
}
