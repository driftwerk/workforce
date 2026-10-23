import {
  useQuery,
  useMutation,
  useQueryClient,
  keepPreviousData,
} from "@tanstack/react-query";
import { identitiesApi, type IdentityListParams } from "@/lib/api/identities";

const KEYS = {
  list: ["identities"] as const,
  detail: (id: string) => ["identities", id] as const,
};

/** Single broad page for non-paginated consumers (pickers, resolvers) that
 * need the whole roster in one request. Bounded by the server's max page so
 * directories beyond this still need a paginated/searched caller. */
const DEFAULT_LIST_PARAMS: IdentityListParams = { limit: 500, offset: 0 };

export function useIdentities(params: IdentityListParams = DEFAULT_LIST_PARAMS) {
  return useQuery({
    queryKey: [...KEYS.list, params],
    queryFn: () => identitiesApi.list(params),
    placeholderData: keepPreviousData,
    staleTime: 30_000,
  });
}

export function useIdentity(id: string) {
  return useQuery({
    queryKey: KEYS.detail(id),
    queryFn: () => identitiesApi.get(id),
    enabled: !!id,
  });
}

export function useCreateIdentity() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: identitiesApi.create,
    onSuccess: () => qc.invalidateQueries({ queryKey: KEYS.list }),
  });
}

export function useUpdateIdentity() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      ...data
    }: { id: string } & Parameters<typeof identitiesApi.update>[1]) =>
      identitiesApi.update(id, data),
    onSuccess: () => qc.invalidateQueries({ queryKey: KEYS.list }),
  });
}

export function useElevateIdentity() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      ...data
    }: { id: string } & Parameters<typeof identitiesApi.elevate>[1]) =>
      identitiesApi.elevate(id, data),
    onSuccess: () => qc.invalidateQueries({ queryKey: KEYS.list }),
  });
}

export function useSetPassword() {
  return useMutation({
    mutationFn: ({ id, password }: { id: string; password: string }) =>
      identitiesApi.setPassword(id, password),
  });
}
