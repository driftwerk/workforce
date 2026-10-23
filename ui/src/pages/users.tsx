import { useState, useMemo, useEffect } from "react";
import { Link, useNavigate } from "react-router";
import {
  useIdentities,
  useCreateIdentity,
  useUpdateIdentity,
  useElevateIdentity,
} from "@/hooks/use-identities";
import { useDebounced } from "@/hooks/use-debounced";
import type { Identity } from "@/lib/api/identities";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Plus, Trash2, ArrowUpCircle, Loader2, AlertCircle } from "lucide-react";

const ROLE_COLORS: Record<string, string> = {
  system_admin: "bg-red-100 text-red-800 dark:bg-red-900 dark:text-red-200",
  manager: "bg-blue-100 text-blue-800 dark:bg-blue-900 dark:text-blue-200",
  member: "bg-gray-100 text-gray-800 dark:bg-gray-900 dark:text-gray-200",
};

const STATUS_COLORS: Record<string, string> = {
  active: "bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-200",
  suspended:
    "bg-yellow-100 text-yellow-800 dark:bg-yellow-900 dark:text-yellow-200",
  auto_provisioned:
    "bg-purple-100 text-purple-800 dark:bg-purple-900 dark:text-purple-200",
};

function RoleBadge({ role }: { role: string }) {
  return (
    <Badge className={ROLE_COLORS[role] ?? ""}>
      {role.replace(/_/g, " ")}
    </Badge>
  );
}

function StatusBadge({ status }: { status: string }) {
  return (
    <Badge variant="outline" className={STATUS_COLORS[status] ?? ""}>
      {status.replace(/_/g, " ")}
    </Badge>
  );
}

function CreateUserDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [role, setRole] = useState("member");
  const [error, setError] = useState("");
  const create = useCreateIdentity();

  const handleCreate = () => {
    setError("");
    create.mutate(
      { display_name: name, email: email || undefined, role },
      {
        onSuccess: () => {
          setOpen(false);
          setName("");
          setEmail("");
          setRole("member");
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Plus className="mr-1.5 h-4 w-4" />
          Add User
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create User</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div>
            <label htmlFor="create-name" className="text-sm font-medium">
              Display name
            </label>
            <Input
              id="create-name"
              placeholder="Display name"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div>
            <label htmlFor="create-email" className="text-sm font-medium">
              Email (optional)
            </label>
            <Input
              id="create-email"
              placeholder="Email"
              type="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
          </div>
          <div>
            <label htmlFor="create-role" className="text-sm font-medium">
              Role
            </label>
            <select
              id="create-role"
              className="w-full rounded-md border px-3 py-2 text-sm bg-background"
              value={role}
              onChange={(e) => setRole(e.target.value)}
            >
              <option value="member">Member</option>
              <option value="manager">Manager</option>
              <option value="system_admin">System Admin</option>
            </select>
          </div>
          {error && (
            <p className="text-sm text-destructive" role="alert">
              {error}
            </p>
          )}
          <Button
            className="w-full"
            onClick={handleCreate}
            disabled={!name.trim() || create.isPending}
          >
            {create.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              "Create"
            )}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function UserRow({ user }: { user: Identity }) {
  const update = useUpdateIdentity();
  const navigate = useNavigate();
  const elevate = useElevateIdentity();
  const [elevateEmail, setElevateEmail] = useState("");
  const [showElevate, setShowElevate] = useState(false);

  const toggleSuspend = () => {
    const newStatus = user.status === "suspended" ? "active" : "suspended";
    update.mutate({ id: user.id, status: newStatus });
  };

  const handleElevate = () => {
    if (!elevateEmail.trim()) return;
    elevate.mutate(
      { id: user.id, email: elevateEmail },
      { onSuccess: () => setShowElevate(false) },
    );
  };

  const linkedPlatforms = Object.keys(user.linked_ids);

  return (
    <tr className="border-b hover:bg-muted/50">
      <td className="px-4 py-3 text-sm font-medium">
        <Link to={`/users/${user.id}`} className="hover:underline text-primary">{user.display_name}</Link>
      </td>
      <td className="px-4 py-3 text-sm text-muted-foreground">
        {user.email ?? "—"}
      </td>
      <td className="px-4 py-3">
        <RoleBadge role={user.role} />
      </td>
      <td className="px-4 py-3">
        <StatusBadge status={user.status} />
      </td>
      <td className="px-4 py-3 text-sm text-muted-foreground">
        {linkedPlatforms.length > 0 ? linkedPlatforms.join(", ") : "—"}
      </td>
      <td className="px-4 py-3">
        <div className="flex gap-1">
          {user.status === "auto_provisioned" &&
            (showElevate ? (
              <div className="flex gap-1">
                <Input
                  size={1}
                  placeholder="Email"
                  aria-label="Elevation email"
                  className="h-7 w-40 text-xs"
                  value={elevateEmail}
                  onChange={(e) => setElevateEmail(e.target.value)}
                />
                <Button
                  size="sm"
                  variant="outline"
                  className="h-7 text-xs"
                  onClick={handleElevate}
                >
                  Go
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-7 text-xs"
                  onClick={() => setShowElevate(false)}
                >
                  Cancel
                </Button>
              </div>
            ) : (
              <Button
                size="sm"
                variant="outline"
                className="h-7 text-xs"
                onClick={() => setShowElevate(true)}
              >
                <ArrowUpCircle className="mr-1 h-3 w-3" />
                Elevate
              </Button>
            ))}
          <Button
            size="sm"
            variant="ghost"
            className="h-7 text-xs"
            onClick={toggleSuspend}
          >
            {user.status === "suspended" ? "Reinstate" : "Suspend"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 text-xs text-destructive"
            onClick={() => navigate(`/users/${user.id}?tab=lifecycle`)}
            aria-label={`Merge or delete ${user.display_name}`}
          >
            <Trash2 className="h-3 w-3" />
          </Button>
        </div>
      </td>
    </tr>
  );
}

const PAGE_SIZE = 50;

export default function UsersPage() {
  const [search, setSearch] = useState("");
  const [page, setPage] = useState(0);
  const debouncedSearch = useDebounced(search, 250);

  const params = useMemo(
    () => ({
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
      search: debouncedSearch.trim() || undefined,
    }),
    [page, debouncedSearch],
  );

  const { data, isLoading, error } = useIdentities(params);

  const identities = data?.identities ?? [];
  const total = data?.total ?? 0;
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  useEffect(() => {
    if (page >= totalPages) setPage(totalPages - 1);
  }, [page, totalPages]);

  function changeSearch(value: string) {
    setSearch(value);
    setPage(0);
  }

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">Users</h1>
          <p className="text-sm text-muted-foreground">{total} identities</p>
        </div>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" asChild>
            <Link to="/users/orphans">Orphaned data</Link>
          </Button>
          <CreateUserDialog />
        </div>
      </div>

      <Input
        placeholder="Search users..."
        aria-label="Search users"
        value={search}
        onChange={(e) => changeSearch(e.target.value)}
        className="max-w-sm"
      />

      {error ? (
        <Card>
          <CardContent className="flex items-center gap-2 p-6 text-destructive">
            <AlertCircle className="h-5 w-5" />
            <p>Failed to load users: {error.message}</p>
          </CardContent>
        </Card>
      ) : (
        <>
          <Card>
            <CardContent className="p-0">
              <table className="w-full" aria-label="Users">
                <thead>
                  <tr className="border-b bg-muted/50">
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Name
                    </th>
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Email
                    </th>
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Role
                    </th>
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Status
                    </th>
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Linked
                    </th>
                    <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                      Actions
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {isLoading ? (
                    <tr>
                      <td
                        colSpan={6}
                        className="px-4 py-8 text-center text-muted-foreground"
                      >
                        Loading...
                      </td>
                    </tr>
                  ) : identities.length === 0 ? (
                    <tr>
                      <td
                        colSpan={6}
                        className="px-4 py-8 text-center text-muted-foreground"
                      >
                        No users found
                      </td>
                    </tr>
                  ) : (
                    identities.map((u) => <UserRow key={u.id} user={u} />)
                  )}
                </tbody>
              </table>
            </CardContent>
          </Card>

          {totalPages > 1 && (
            <div className="flex items-center justify-between text-sm text-muted-foreground">
              <span>
                Page {page + 1} of {totalPages}
              </span>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={page === 0}
                  onClick={() => setPage((p) => p - 1)}
                >
                  Previous
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={page >= totalPages - 1}
                  onClick={() => setPage((p) => p + 1)}
                >
                  Next
                </Button>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
