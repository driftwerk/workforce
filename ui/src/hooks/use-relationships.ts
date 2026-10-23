import { useQuery } from "@tanstack/react-query";
import { relationshipsApi } from "@/lib/api/relationships";

export function useMyRelationship(days = 90) {
  return useQuery({
    queryKey: ["my-relationship", days],
    queryFn: () => relationshipsApi.mine(days),
    staleTime: 30_000,
  });
}
