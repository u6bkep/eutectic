//! The semantic state buffer, CPU side (renderer-spec §5).
//!
//! A small array of per-id **flag words** indexed by the scene's compact
//! semantic ids ([`Scene::semantics`](super::scene::Scene::semantics)).
//! Hover, selection, and net-highlight changes are **one-word writes**
//! followed by a texture re-render — no scene rebuild. The GPU mirror is a
//! storage buffer the coverage vertex shader fetches (flagged fragments
//! write the G channel as well as R); the renderer re-uploads when the
//! [`generation`](SemanticStates::generation) counter moved (the §7 damage
//! input). Cross-view highlight is the same write observed by both panes'
//! renders.

/// The pointer is over this entity/net.
pub const FLAG_HOVERED: u32 = 1 << 0;
/// Committed selection.
pub const FLAG_SELECTED: u32 = 1 << 1;
/// Emphasis tier (net highlight, findings focus; room to grow above).
pub const FLAG_EMPHASIS: u32 = 1 << 2;

/// Per-semantic-id flag words + a generation counter for damage tracking.
#[derive(Debug)]
pub struct SemanticStates {
    /// Process-unique buffer identity (see [`id`](SemanticStates::id)):
    /// generations from *different* `SemanticStates` are not comparable, so
    /// the renderer keys its GPU upload on `(id, generation)` — a fresh
    /// instance (doc swap) or a diverged clone can never alias a previously
    /// uploaded generation number and skip its upload.
    id: u64,
    words: Vec<u32>,
    generation: u64,
}

fn next_states_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A clone is a *new* flag buffer that happens to start with the same
/// content — it diverges independently, so it mints a fresh identity.
impl Clone for SemanticStates {
    fn clone(&self) -> SemanticStates {
        SemanticStates {
            id: next_states_id(),
            words: self.words.clone(),
            generation: self.generation,
        }
    }
}

/// Content equality (words + generation); the buffer identity is excluded.
impl PartialEq for SemanticStates {
    fn eq(&self, other: &SemanticStates) -> bool {
        self.words == other.words && self.generation == other.generation
    }
}

impl SemanticStates {
    /// All-clear words for `len` semantic ids (index 0 is the chrome
    /// sentinel — flagging it is a no-op by convention, enforced in
    /// [`set_word`](SemanticStates::set_word)).
    pub fn new(len: usize) -> SemanticStates {
        SemanticStates {
            id: next_states_id(),
            words: vec![0; len.max(1)],
            generation: 0,
        }
    }

    /// The process-unique identity of this flag buffer (fresh per
    /// construction and per clone). Pair it with
    /// [`generation`](SemanticStates::generation) when caching uploads.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sized for a scene's semantic table.
    pub fn for_scene(scene: &super::scene::Scene) -> SemanticStates {
        SemanticStates::new(scene.semantics.len())
    }

    /// The one-word update API: set id's whole flag word. Bumps the
    /// generation only on a real change; writes to the chrome sentinel (id
    /// 0) or out of range are ignored (chrome never highlights). Returns
    /// whether anything changed.
    pub fn set_word(&mut self, id: u32, word: u32) -> bool {
        let i = id as usize;
        if i == 0 || i >= self.words.len() || self.words[i] == word {
            return false;
        }
        self.words[i] = word;
        self.generation += 1;
        true
    }

    /// Set or clear individual flags on one id (sugar over
    /// [`set_word`](SemanticStates::set_word)).
    pub fn set_flags(&mut self, id: u32, flags: u32, on: bool) -> bool {
        let i = id as usize;
        if i >= self.words.len() {
            return false;
        }
        let w = self.words[i];
        self.set_word(id, if on { w | flags } else { w & !flags })
    }

    /// Clear every flag word (selection cleared, hover left). One bump.
    pub fn clear(&mut self) {
        if self.words.iter().any(|&w| w != 0) {
            self.words.iter_mut().for_each(|w| *w = 0);
            self.generation += 1;
        }
    }

    /// The flag word for `id` (0 when out of range).
    pub fn word(&self, id: u32) -> u32 {
        self.words.get(id as usize).copied().unwrap_or(0)
    }

    /// The raw words, for the GPU upload.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Damage-key input: moves iff any word changed since construction.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_word_updates_bump_generation_only_on_change() {
        let mut s = SemanticStates::new(4);
        assert_eq!(s.generation(), 0);
        assert!(s.set_word(2, FLAG_SELECTED));
        assert_eq!(s.generation(), 1);
        assert!(!s.set_word(2, FLAG_SELECTED), "idempotent write is free");
        assert_eq!(s.generation(), 1);
        assert!(s.set_flags(2, FLAG_HOVERED, true));
        assert_eq!(s.word(2), FLAG_SELECTED | FLAG_HOVERED);
        assert!(s.set_flags(2, FLAG_SELECTED, false));
        assert_eq!(s.word(2), FLAG_HOVERED);
        assert_eq!(s.generation(), 3);
    }

    #[test]
    fn chrome_sentinel_and_out_of_range_are_ignored() {
        let mut s = SemanticStates::new(2);
        assert!(!s.set_word(0, FLAG_SELECTED), "chrome never flags");
        assert!(!s.set_word(99, FLAG_SELECTED));
        assert_eq!(s.generation(), 0);
        assert_eq!(s.word(99), 0);
    }

    #[test]
    fn clear_is_one_bump_and_idempotent() {
        let mut s = SemanticStates::new(8);
        s.set_word(1, FLAG_EMPHASIS);
        s.set_word(5, FLAG_HOVERED);
        let g = s.generation();
        s.clear();
        assert_eq!(s.generation(), g + 1);
        s.clear();
        assert_eq!(s.generation(), g + 1);
        assert!(s.words().iter().all(|&w| w == 0));
    }

    #[test]
    fn buffer_identity_is_unique_per_instance_and_per_clone() {
        // Two fresh buffers at the same generation must not alias in the
        // renderer's (id, generation) upload key — the doc-swap scenario.
        let mut a = SemanticStates::new(8);
        let mut b = SemanticStates::new(8);
        assert_ne!(a.id(), b.id());
        a.set_word(1, FLAG_SELECTED);
        b.set_word(2, FLAG_SELECTED);
        assert_eq!(a.generation(), b.generation());
        assert_ne!((a.id(), a.generation()), (b.id(), b.generation()));
        // A clone diverges independently, so it is a new buffer identity —
        // equal content, distinct id.
        let c = a.clone();
        assert_ne!(c.id(), a.id());
        assert_eq!(c, a);
    }
}
