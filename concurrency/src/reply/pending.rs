//! `PendingReplies<K, R>`: a bounded table of deferred tokens by key
//! (docs/design/deferred-replies.md §7).

use super::ReplyTo;
use std::hash::Hash;

/// Why [`PendingReplies::insert`] handed the token back. The handler decides
/// the answer (typically `token.overloaded()` or a domain error) and still
/// owns the token.
///
/// The enum is `#[non_exhaustive]`; match on [`kind`](Self::kind) (or take
/// both with [`into_parts`](Self::into_parts)) so the token is answered
/// whatever the reason, including one added later, instead of being
/// dropped in a wildcard arm (which answers the caller `ReplyDropped`):
///
/// ```ignore
/// if let Err(refused) = table.insert(id, token) {
///     let (kind, token) = refused.into_parts();
///     match kind {
///         InsertErrorKind::Duplicate => token.reply(Err(Error::DuplicateId)),
///         _ => token.overloaded(), // Full, and any later reason
///     }
/// }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum InsertError<R: Send + 'static> {
    /// A live token is already stored under this key; it is kept.
    Duplicate(ReplyTo<R>),
    /// The table holds `cap` live tokens.
    Full(ReplyTo<R>),
}

/// Why an insert was refused, without the token ([`InsertError::kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InsertErrorKind {
    /// A live token is already stored under this key.
    Duplicate,
    /// The table holds `cap` live tokens.
    Full,
}

impl<R: Send + 'static> InsertError<R> {
    /// Why the token was handed back.
    pub fn kind(&self) -> InsertErrorKind {
        match self {
            Self::Duplicate(_) => InsertErrorKind::Duplicate,
            Self::Full(_) => InsertErrorKind::Full,
        }
    }
    /// The token handed back.
    pub fn into_inner(self) -> ReplyTo<R> {
        match self {
            Self::Duplicate(token) | Self::Full(token) => token,
        }
    }
    /// The reason and the token together: every refusal can be answered.
    pub fn into_parts(self) -> (InsertErrorKind, ReplyTo<R>) {
        (self.kind(), self.into_inner())
    }
}

/// Deferred tokens by key, capped: retained entries never exceed `cap`,
/// however fast tokens expire. Terminal tokens (answered, expired, abandoned)
/// are purged on every insert and skipped by `take`.
///
/// Experimental in 0.8: a utility shaped by one client (requests by id
/// answered when a peer responds); it may change before 1.0.
///
/// Entries keep insertion order: [`drain`](Self::drain) yields them oldest
/// first, the same in every process, so answering a drained table is
/// reproducible under the simulator (no per-process hash seed decides it).
/// The table is a scan (insert already purges the whole table, and `cap`
/// bounds it): `take` and `insert` are linear in the held entries.
pub struct PendingReplies<K, R: Send + 'static> {
    cap: usize,
    tokens: Vec<(K, ReplyTo<R>)>,
}

impl<K, R: Send + 'static> std::fmt::Debug for PendingReplies<K, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingReplies")
            .field("cap", &self.cap)
            .field("retained", &self.tokens.len())
            .finish()
    }
}

impl<K: Hash + Eq, R: Send + 'static> PendingReplies<K, R> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            tokens: Vec::new(),
        }
    }

    /// Purge terminal tokens, then insert. A duplicate key (with a live
    /// token) or a full table hands the new token back.
    pub fn insert(&mut self, key: K, token: ReplyTo<R>) -> Result<(), InsertError<R>> {
        self.purge();
        if self.tokens.iter().any(|(k, _)| *k == key) {
            return Err(InsertError::Duplicate(token));
        }
        if self.tokens.len() >= self.cap {
            return Err(InsertError::Full(token));
        }
        self.tokens.push((key, token));
        Ok(())
    }

    /// Remove and return the live token under `key`; a terminal one is
    /// removed and skipped.
    pub fn take(&mut self, key: &K) -> Option<ReplyTo<R>> {
        let at = self.tokens.iter().position(|(k, _)| k == key)?;
        let (_, token) = self.tokens.remove(at);
        (!token.is_closed()).then_some(token)
    }

    /// Remove every terminal token.
    fn purge(&mut self) {
        self.tokens.retain(|(_, token)| !token.is_closed());
    }

    /// Live (non-terminal) tokens held: a scan, so a token whose caller
    /// just left no longer counts.
    pub fn len(&self) -> usize {
        self.tokens.iter().filter(|(_, t)| !t.is_closed()).count()
    }

    /// Whether no live token is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take every live token, oldest first (for example to answer them in
    /// `stopped()`).
    pub fn drain(&mut self) -> impl Iterator<Item = (K, ReplyTo<R>)> + '_ {
        self.tokens
            .drain(..)
            .filter(|(_, token)| !token.is_closed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reply::{reply_pair, Deadline};
    use crate::ActorError;

    fn token() -> (ReplyTo<u32>, crate::reply::ReplyReceiver<u32>) {
        let (reply, rx) = reply_pair::<u32>(Deadline::Never);
        (reply.defer().unwrap(), rx)
    }

    #[test]
    fn bounded_with_duplicates_and_purge() {
        let mut table = PendingReplies::with_capacity(2);
        let (a, ra) = token();
        let (b, rb) = token();
        let (c, rc) = token();
        let (dup, rdup) = token();
        table.insert(1, a).unwrap();
        table.insert(2, b).unwrap();
        let full = table.insert(3, c).unwrap_err();
        assert!(matches!(full, InsertError::Full(_)));
        full.into_inner().overloaded();
        assert!(matches!(rc.recv(), Err(ActorError::Overloaded)));
        let duplicate = table.insert(1, dup).unwrap_err();
        assert!(matches!(duplicate, InsertError::Duplicate(_)));
        drop(duplicate);
        assert!(matches!(rdup.recv(), Err(ActorError::ReplyDropped)));
        // A caller that left frees its entry on the next insert.
        drop(rb);
        assert_eq!(
            table.tokens.len(),
            2,
            "terminal tokens stay until an insert"
        );
        assert_eq!(table.len(), 1);
        let (d, rd) = token();
        table.insert(4, d).unwrap();
        assert_eq!(table.tokens.len(), 2);
        assert_eq!(table.len(), 2);
        assert!(table.take(&2).is_none());
        table.take(&1).unwrap().reply(10);
        table.take(&4).unwrap().reply(40);
        assert_eq!(ra.recv().unwrap(), 10);
        assert_eq!(rd.recv().unwrap(), 40);
    }
    /// #261: draining follows insertion order, so two tables built the
    /// same way (in the old `HashMap`, two different `RandomState` seeds)
    /// drain identically, oldest first, with taken and terminal entries
    /// skipped.
    #[test]
    fn drain_follows_insertion_order() {
        let keys: Vec<u64> = (0..64).map(|i| (i * 7_919) % 1_000).collect();
        let build = || {
            let mut table = PendingReplies::with_capacity(keys.len());
            let mut receivers = Vec::new();
            for &k in &keys {
                let (t, rx) = token();
                table.insert(k, t).unwrap();
                receivers.push(rx);
            }
            table.take(&keys[3]).unwrap().reply(0);
            drop(receivers.remove(10)); // abandoned: terminal, skipped
            (table, receivers)
        };
        let (mut first, _a) = build();
        let (mut second, _b) = build();
        let once: Vec<u64> = first
            .drain()
            .map(|(k, t)| {
                drop(t);
                k
            })
            .collect();
        let twice: Vec<u64> = second
            .drain()
            .map(|(k, t)| {
                drop(t);
                k
            })
            .collect();
        let expected: Vec<u64> = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3 && *i != 10)
            .map(|(_, k)| *k)
            .collect();
        assert_eq!(once, expected);
        assert_eq!(twice, expected);
    }

    /// Every refusal hands back its reason and its token together, so a
    /// caller can answer the token for any reason, known or not.
    #[test]
    fn a_refusal_gives_its_kind_and_its_token() {
        let mut table = PendingReplies::with_capacity(1);
        let (a, ra) = token();
        let (dup, rdup) = token();
        let (full, rfull) = token();
        table.insert(1, a).unwrap();
        let refused = table.insert(1, dup).unwrap_err();
        assert_eq!(refused.kind(), InsertErrorKind::Duplicate);
        let (kind, token) = refused.into_parts();
        assert_eq!(kind, InsertErrorKind::Duplicate);
        token.overloaded();
        assert!(matches!(rdup.recv(), Err(ActorError::Overloaded)));
        let refused = table.insert(2, full).unwrap_err();
        let (kind, token) = refused.into_parts();
        assert_eq!(kind, InsertErrorKind::Full);
        token.reply(9);
        assert_eq!(rfull.recv().unwrap(), 9);
        table.take(&1).unwrap().reply(1);
        assert_eq!(ra.recv().unwrap(), 1);
    }
}
