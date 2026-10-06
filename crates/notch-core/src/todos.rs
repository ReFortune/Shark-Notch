//! The to-do list that sits beside the Pomodoro timer: short tasks, one of which can be "the one
//! I'm working on", and a count of the focus sessions spent on each.
//!
//! Pure data with a JSON form. The text is whatever the user typed or pasted, so it is cleaned
//! (single line, no control characters, bounded) before it is stored.

use serde::{Deserialize, Serialize};

pub const MAX_TODOS: usize = 60;
pub const MAX_TITLE: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: u32,
    pub title: String,
    pub done: bool,
    /// Focus sessions completed while this was the current task.
    pub sessions: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoList {
    items: Vec<Todo>,
    next_id: u32,
    current: Option<u32>,
}

/// Single line, no control characters, collapsed whitespace, at most [`MAX_TITLE`] characters.
pub fn clean_title(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else if !c.is_control() {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    if out.chars().count() > MAX_TITLE {
        out = out
            .chars()
            .take(MAX_TITLE)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    (!out.is_empty()).then_some(out)
}

impl TodoList {
    pub fn items(&self) -> &[Todo] {
        &self.items
    }

    pub fn current(&self) -> Option<&Todo> {
        self.current
            .and_then(|id| self.items.iter().find(|t| t.id == id))
    }

    pub fn current_id(&self) -> Option<u32> {
        self.current
    }

    pub fn open_count(&self) -> usize {
        self.items.iter().filter(|t| !t.done).count()
    }

    pub fn done_count(&self) -> usize {
        self.items.len() - self.open_count()
    }

    /// Item indices in display order: open tasks (oldest first), then finished ones.
    pub fn display_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.items.len()).collect();
        idx.sort_by_key(|&i| self.items[i].done);
        idx
    }

    /// Add a task; `None` if the text is empty or the list is full.
    pub fn add(&mut self, raw: &str) -> Option<u32> {
        let title = clean_title(raw)?;
        if self.items.len() >= MAX_TODOS {
            return None;
        }
        let id = self.next_id.max(1);
        self.next_id = id.wrapping_add(1).max(1);
        self.items.push(Todo {
            id,
            title,
            done: false,
            sessions: 0,
        });
        Some(id)
    }

    /// Tick or untick a task. Finishing the current task makes it no longer current.
    pub fn toggle(&mut self, id: u32) -> bool {
        let Some(t) = self.items.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        t.done = !t.done;
        if t.done && self.current == Some(id) {
            self.current = None;
        }
        true
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.items.len();
        self.items.retain(|t| t.id != id);
        if self.current == Some(id) {
            self.current = None;
        }
        self.items.len() != before
    }

    /// Make a task the one being worked on (clicking the current one again clears it). Finished
    /// tasks cannot be current.
    pub fn select(&mut self, id: u32) {
        match self.items.iter().find(|t| t.id == id) {
            Some(t) if !t.done => {
                self.current = if self.current == Some(id) {
                    None
                } else {
                    Some(id)
                };
            }
            _ => {}
        }
    }

    /// A focus session finished: credit it to the current task. Returns that task's id.
    pub fn credit_session(&mut self) -> Option<u32> {
        let id = self.current?;
        let t = self.items.iter_mut().find(|t| t.id == id)?;
        t.sessions = t.sessions.saturating_add(1);
        Some(id)
    }

    /// Remove every finished task.
    pub fn clear_done(&mut self) -> usize {
        let before = self.items.len();
        self.items.retain(|t| !t.done);
        before - self.items.len()
    }

    /// Repair a list read from disk: bounded, unique ids, a current task that exists.
    pub fn sanitized(mut self) -> TodoList {
        self.items.truncate(MAX_TODOS);
        let mut seen = std::collections::HashSet::new();
        self.items.retain(|t| seen.insert(t.id) && t.id != 0);
        for t in &mut self.items {
            t.title = clean_title(&t.title).unwrap_or_else(|| "(untitled)".into());
        }
        let max_id = self.items.iter().map(|t| t.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1)).max(1);
        if self
            .current
            .is_some_and(|c| !self.items.iter().any(|t| t.id == c && !t.done))
        {
            self.current = None;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(
            clean_title("  write   the\tdocs \n"),
            Some("write the docs".into())
        );
        assert_eq!(clean_title("a\u{7}b"), Some("ab".into()));
        assert_eq!(clean_title("   \n "), None);
        assert_eq!(clean_title(""), None);
        let long = "é".repeat(500);
        assert_eq!(clean_title(&long).unwrap().chars().count(), MAX_TITLE);
        let spaced = format!("{} tail", "x".repeat(MAX_TITLE - 1));
        assert_eq!(
            clean_title(&spaced).unwrap(),
            "x".repeat(MAX_TITLE - 1),
            "no trailing space left behind"
        );
    }

    #[test]
    fn adding_toggling_and_removing() {
        let mut l = TodoList::default();
        let a = l.add("first").unwrap();
        let b = l.add("second").unwrap();
        assert_ne!(a, b);
        assert_eq!(l.add("   "), None);
        assert_eq!(l.open_count(), 2);
        assert!(l.toggle(a));
        assert_eq!((l.open_count(), l.done_count()), (1, 1));
        assert!(!l.toggle(999));
        assert!(l.remove(b));
        assert!(!l.remove(b));
        assert_eq!(l.items().len(), 1);
    }

    #[test]
    fn open_tasks_are_listed_before_finished_ones() {
        let mut l = TodoList::default();
        let a = l.add("a").unwrap();
        l.add("b").unwrap();
        l.add("c").unwrap();
        l.toggle(a);
        let titles: Vec<&str> = l
            .display_order()
            .into_iter()
            .map(|i| l.items()[i].title.as_str())
            .collect();
        assert_eq!(titles, vec!["b", "c", "a"]);
    }

    #[test]
    fn the_current_task_collects_sessions_and_clears_when_finished() {
        let mut l = TodoList::default();
        let a = l.add("a").unwrap();
        let b = l.add("b").unwrap();
        assert_eq!(l.credit_session(), None, "no current task");
        l.select(a);
        assert_eq!(l.current().map(|t| t.id), Some(a));
        assert_eq!(l.credit_session(), Some(a));
        assert_eq!(l.credit_session(), Some(a));
        assert_eq!(l.items()[0].sessions, 2);
        l.select(b);
        assert_eq!(l.current_id(), Some(b), "selecting another switches");
        l.select(b);
        assert_eq!(l.current_id(), None, "selecting the current one clears it");
        l.select(a);
        l.toggle(a);
        assert_eq!(
            l.current_id(),
            None,
            "finishing the current task releases it"
        );
        l.select(a);
        assert_eq!(l.current_id(), None, "a finished task cannot be current");
    }

    #[test]
    fn removing_the_current_task_clears_it() {
        let mut l = TodoList::default();
        let a = l.add("a").unwrap();
        l.select(a);
        l.remove(a);
        assert_eq!(l.current_id(), None);
    }

    #[test]
    fn the_list_is_bounded() {
        let mut l = TodoList::default();
        for i in 0..MAX_TODOS {
            assert!(l.add(&format!("t{i}")).is_some());
        }
        assert_eq!(l.add("one too many"), None);
        assert_eq!(l.items().len(), MAX_TODOS);
    }

    #[test]
    fn clear_done_removes_only_finished_tasks() {
        let mut l = TodoList::default();
        let a = l.add("a").unwrap();
        l.add("b").unwrap();
        l.toggle(a);
        assert_eq!(l.clear_done(), 1);
        assert_eq!(l.items().len(), 1);
        assert_eq!(l.clear_done(), 0);
    }

    #[test]
    fn json_round_trip_and_repair_of_a_damaged_file() {
        let mut l = TodoList::default();
        let a = l.add("keep me").unwrap();
        l.select(a);
        l.credit_session();
        let json = serde_json::to_string(&l).unwrap();
        let back: TodoList = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sanitized(), l);

        // Duplicate ids, a dangling current task, an empty title, too many items, id 0.
        let bad = TodoList {
            items: vec![
                Todo {
                    id: 1,
                    title: "a".into(),
                    done: false,
                    sessions: 0,
                },
                Todo {
                    id: 1,
                    title: "dup".into(),
                    done: false,
                    sessions: 0,
                },
                Todo {
                    id: 0,
                    title: "zero".into(),
                    done: false,
                    sessions: 0,
                },
                Todo {
                    id: 2,
                    title: "  ".into(),
                    done: false,
                    sessions: 0,
                },
            ],
            next_id: 0,
            current: Some(77),
        }
        .sanitized();
        assert_eq!(bad.items().len(), 2);
        assert_eq!(bad.items()[1].title, "(untitled)");
        assert_eq!(bad.current_id(), None);
        let mut bad = bad;
        let id = bad.add("new").unwrap();
        assert!(
            bad.items().iter().filter(|t| t.id == id).count() == 1,
            "ids stay unique"
        );
    }

    #[test]
    fn unknown_or_missing_fields_do_not_break_loading() {
        assert_eq!(
            serde_json::from_str::<TodoList>("{}").unwrap(),
            TodoList::default()
        );
        let l: TodoList =
            serde_json::from_str(r#"{"items":[],"next_id":1,"future_field":[1,2,3]}"#).unwrap();
        assert!(l.items().is_empty());
        assert!(serde_json::from_str::<TodoList>("not json").is_err());
    }
}
