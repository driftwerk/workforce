/** Who reaches an owned resource beyond its owner and its grantees, in the
 *  words the skill routes speak. */
export type Visibility = "private" | "team" | "everyone";

/** The choices a visibility control offers, closed first. */
export const VISIBILITIES: Visibility[] = ["private", "team", "everyone"];

const LABELS: Record<Visibility, string> = {
  private: "Private",
  team: "My teams",
  everyone: "Everyone",
};

/** `team` follows the *owner's* teams and is resolved every time someone asks,
 *  so the middle choice reaches nobody while its owner is in no team — which the
 *  help says, because the page cannot know whose teams the viewer shares. */
const HELP: Record<Visibility, string> = {
  private: "Only you and the people you share it with",
  team: "Anyone in a team with you can run it — nobody, until you join one",
  everyone: "Anyone on this install can run it",
};

/** The same reaches as a definition surface spells them: a definition row keeps
 *  the stored word `global` where a skill record says `everyone`. */
const DEFINITION_LABELS: Record<Visibility, string> = {
  private: "Private",
  team: "My teams",
  everyone: "Global",
};

export function visibilityLabel(visibility: Visibility): string {
  return LABELS[visibility];
}

export function visibilityHelp(visibility: Visibility): string {
  return HELP[visibility];
}

/** The reaches a resource may be given. A resource nobody owns has no owner
 *  whose teams could be resolved, so the middle choice is not among them — the
 *  definition routes refuse it outright. */
export function visibilityChoices(ownerless: boolean): Visibility[] {
  return ownerless ? VISIBILITIES.filter((v) => v !== "team") : VISIBILITIES;
}

/** The reach a definition row's stored word means. An absent or unrecognised
 *  word fails closed to private, exactly as the store reads it. */
export function definitionVisibility(visibility: string | undefined): Visibility {
  if (visibility === "global") return "everyone";
  if (visibility === "team") return "team";
  return "private";
}

/** The word a definition save stores for a chosen reach: the definition routes
 *  speak the stored vocabulary and refuse `everyone`. */
export function definitionWord(
  visibility: Visibility,
): "private" | "team" | "global" {
  return visibility === "everyone" ? "global" : visibility;
}

/** How a definition's reach reads on its card. */
export function definitionLabel(visibility: string | undefined): string {
  return DEFINITION_LABELS[definitionVisibility(visibility)];
}

/** Whoever the page is acting as, as far as standing is concerned. */
interface Actor {
  id: string;
  role: string;
}

/** Whether `actor` may hand the resource `owner` holds to somebody else: its
 *  owner, or an administrator reaching past them. A `manage` grantee edits a
 *  resource and never gives it away, so the control is not offered where the
 *  route would only refuse it. A resource nobody owns has no ownership to
 *  pass on. */
export function canTransfer(
  actor: Actor | null | undefined,
  owner: string | null | undefined,
): boolean {
  return !!owner && (actor?.id === owner || actor?.role === "system_admin");
}

/** The badge a definition's reach earns, or `undefined` when it is the private
 *  default — a listing marks what widens the default, not the default itself. */
export function definitionReach(visibility: string | undefined): string | undefined {
  const reach = definitionVisibility(visibility);
  return reach === "private" ? undefined : DEFINITION_LABELS[reach];
}
