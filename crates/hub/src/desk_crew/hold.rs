//! A boss's hold on a session at Act fully (DW-761, 25 Nov: the boss held
//! a merge "for the person's word" that no directive asked for, and never
//! asked them).
//!
//! - **Named.** A hold is a directive's: `crew.nudge` with `directive_id`
//!   (one of the person's directives) and `until`. One with no directive,
//!   however it says to hold, wait or not push, is refused, so no gate is
//!   ever invented.
//! - **Asked.** Its question goes to the person at once (`crew.ask_person`
//!   about the session, asked first), as a hold below Act fully does.
//! - **Ended** when the person answers its question (`person_answers`),
//!   the directive is withdrawn (`directive_withdrawn`; a withdrawn
//!   directive ends every hold of its), or the boss records its condition
//!   met (`crew.nudge` with `hold_met`), which closes its question too.
//! - **Kept** in Postgres (`boss_holds`), so a restart never drops one.

use serde_json::Value;
use uuid::Uuid;
use workforce_common::oversight::Level;
use workforce_desk::open_item::{ClosedBy, OpenItem};

use super::actions::{Action, Target};
use super::items::{asks_about, HOLD_ASKS};
use super::CrewHub;

/// Why a hold at Act fully with no directive is refused.
pub(crate) const NAMES_DIRECTIVE: &str = "At Act fully a hold is only ever a directive's: name \
    the person's directive that needs it (directive_id, from crew.directives) and what ends it \
    (until). With no directive that needs it, there is no hold: carry on with the work.";

/// Why a hold naming what isn't one of the person's directives is refused.
const NOT_A_DIRECTIVE: &str = "That isn't one of the person's directives (crew.directives lists \
    them with their ids): a hold names the one that needs it.";

/// Why `hold_met` on a session not held on a condition is refused.
const NO_CONDITION: &str =
    "That session isn't held on a condition, so hold_met has nothing to end.";

pub(crate) use workforce_desk::hold_store::{Hold, Until};

/// What a boss's message does to its session's hold, once it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldChange {
    Hold(Hold),
    /// Its condition was met, as the boss says.
    Met {
        session: Uuid,
        said: String,
    },
}

fn said<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

impl CrewHub {
    /// What `boss`'s `action`, sent with `input` at `level`, does to a
    /// session's hold, checked before it is sent: a hold on the person's
    /// word carries its question, asked first; at Act fully it also names
    /// the directive that needs it, and `hold_met` ends only a condition's
    /// hold.
    pub(crate) async fn hold_asks(
        &self,
        action: &Action,
        input: &Value,
        boss: &str,
        level: Level,
    ) -> Result<Option<HoldChange>, String> {
        let Action::Nudge {
            to: Target::Session(s),
            message,
            ..
        } = action
        else {
            return Ok(None);
        };
        if let Some(met) = said(input, "hold_met") {
            return match self.hold_on(*s).await? {
                Some(Hold {
                    until: Until::Condition(_),
                    ..
                }) => Ok(Some(HoldChange::Met {
                    session: *s,
                    said: met.to_string(),
                })),
                _ => Err(NO_CONDITION.into()),
            };
        }
        let directive = said(input, "directive_id");
        let until = said(input, "until").and_then(Until::of);
        let full = level == Level::Full;
        let holds = workforce_desk::line::holds_for_person(message)
            || (full
                && (directive.is_some()
                    || until.is_some()
                    || workforce_desk::line::holds_session(message)));
        if !holds {
            return Ok(None);
        }
        let items = self
            .desk
            .store()
            .items_not_closed(Some(boss))
            .await
            .map_err(Self::item_err)?;
        let question = items
            .iter()
            .filter(|i| asks_about(i, *s))
            .max_by_key(|i| i.opened_at);
        if !full {
            return question.map(|_| None).ok_or_else(|| HOLD_ASKS.into());
        }
        let directive_id = directive
            .and_then(|d| d.parse::<Uuid>().ok())
            .ok_or(NAMES_DIRECTIVE)?;
        let question: &OpenItem = question.ok_or(HOLD_ASKS)?;
        self.desk
            .store()
            .directive_of(&question.owner, directive_id)
            .await
            .map_err(Self::item_err)?
            .ok_or(NOT_A_DIRECTIVE)?;
        Ok(Some(HoldChange::Hold(Hold {
            session: *s,
            boss: boss.to_string(),
            owner: question.owner.clone(),
            directive_id,
            question: question.id,
            until: until.unwrap_or(Until::PersonAnswers),
        })))
    }

    /// Keeps `change`, its message sent: the hold kept, or ended with its
    /// question closed.
    pub(crate) async fn hold_changed(&self, change: HoldChange) {
        let store = self.desk.store();
        match change {
            HoldChange::Hold(h) => {
                if let Err(e) = store.hold_put(&h).await {
                    tracing::warn!(session = %h.session, error = %format!("{e:#}"), "a hold wasn't kept");
                }
            }
            HoldChange::Met { session, said } => {
                let h = match store.hold_end(session).await {
                    Ok(Some(h)) => h,
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!(%session, error = %format!("{e:#}"), "a met hold wasn't ended");
                        return;
                    }
                };
                let why = format!("Its condition was met: {said}");
                if let Err(e) = store.item_close(h.question, ClosedBy::Boss, &why).await {
                    tracing::warn!(%session, error = %format!("{e:#}"), "a met hold's question wasn't closed");
                }
            }
        }
    }

    /// The hold `session` is under now, if any: one whose directive was
    /// withdrawn, or whose question the person answered when that ends it,
    /// is over and ended. Why it can't be said, when its hold can't be read.
    pub(crate) async fn hold_on(&self, session: Uuid) -> Result<Option<Hold>, String> {
        let store = self.desk.store();
        let Some(h) = store.hold_of(session).await.map_err(|e| {
            tracing::warn!(%session, error = %format!("{e:#}"), "a session's hold couldn't be read");
            "Whether it is held couldn't be read, so it stays as it is for now.".to_string()
        })?
        else {
            return Ok(None);
        };
        let withdrawn = matches!(store.directive_of(&h.owner, h.directive_id).await, Ok(None));
        let answered = h.until == Until::PersonAnswers
            && store
                .items_not_closed(Some(&h.boss))
                .await
                .is_ok_and(|items| !items.iter().any(|i| i.id == h.question));
        if withdrawn || answered {
            if let Err(e) = store.hold_end(session).await {
                tracing::warn!(%session, error = %format!("{e:#}"), "an ended hold wasn't dropped");
            }
            return Ok(None);
        }
        Ok(Some(h))
    }
}
