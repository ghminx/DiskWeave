use crate::navigation::{History, ViewState};

pub const SPLIT_DIVIDER: f32 = 8.0;

pub fn split_widths(width: f32, fraction: f32) -> Option<[f32; 2]> {
    const MIN: f32 = 320.0;
    let lists = width - SPLIT_DIVIDER;
    if lists < 2.0 * MIN {
        return None;
    }
    let left = (lists * fraction).clamp(MIN, lists - MIN);
    Some([left, lists - left])
}

#[derive(Default)]
pub struct PaneMemory {
    pub view: Option<ViewState>,
    pub history: History,
}

impl PaneMemory {
    pub fn at(view: ViewState) -> Self {
        Self {
            view: Some(view),
            history: History::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Side {
    #[default]
    Left,
    Right,
}

impl Side {
    pub fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Default)]
struct FolderTab {
    id: u64,
    left: PaneMemory,
    right: Option<PaneMemory>,
    side: Side,
}

impl FolderTab {
    fn pane(&self, side: Side) -> &PaneMemory {
        match side {
            Side::Left => &self.left,
            Side::Right => self.right.as_ref().expect("열린 오른쪽 화면"),
        }
    }

    fn active(&mut self) -> &mut PaneMemory {
        match self.side {
            Side::Left => &mut self.left,
            Side::Right => self.right.as_mut().expect("열린 오른쪽 화면"),
        }
    }
}

pub struct FolderTabs {
    tabs: Vec<FolderTab>,
    selected: usize,
    next_id: u64,
}

impl Default for FolderTabs {
    fn default() -> Self {
        Self {
            tabs: vec![FolderTab::default()],
            selected: 0,
            next_id: 1,
        }
    }
}

impl FolderTabs {
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn id(&self) -> u64 {
        self.tabs[self.selected].id
    }

    pub fn side(&self) -> Side {
        self.tabs[self.selected].side
    }

    pub fn is_split(&self) -> bool {
        self.tabs[self.selected].right.is_some()
    }

    pub fn other(&self) -> Option<&ViewState> {
        self.is_split()
            .then(|| {
                self.tabs[self.selected]
                    .pane(self.side().other())
                    .view
                    .as_ref()
            })
            .flatten()
    }

    pub fn tab_view(&self, index: usize) -> Option<&ViewState> {
        let tab = &self.tabs[index];
        tab.pane(tab.side).view.as_ref()
    }

    pub fn open_tab(&mut self, current: PaneMemory) {
        *self.tabs[self.selected].active() = current;
        self.selected = self.tabs.len();
        self.tabs.push(FolderTab {
            id: self.next_id,
            ..FolderTab::default()
        });
        self.next_id += 1;
    }

    pub fn open_right(&mut self, current: PaneMemory) {
        let tab = &mut self.tabs[self.selected];
        if tab.side == Side::Left {
            tab.left = current;
        }
        tab.right = Some(PaneMemory::default());
        tab.side = Side::Right;
    }

    pub fn select_tab(&mut self, index: usize, current: PaneMemory) -> PaneMemory {
        *self.tabs[self.selected].active() = current;
        self.selected = index;
        std::mem::take(self.tabs[index].active())
    }

    pub fn select_other(&mut self, current: PaneMemory) -> PaneMemory {
        let tab = &mut self.tabs[self.selected];
        *tab.active() = current;
        tab.side = tab.side.other();
        std::mem::take(tab.active())
    }

    pub fn close_right(&mut self) -> Option<PaneMemory> {
        let tab = &mut self.tabs[self.selected];
        tab.right = None;
        let resume = tab.side == Side::Right;
        tab.side = Side::Left;
        resume.then(|| std::mem::take(&mut tab.left))
    }

    pub fn close_tab(&mut self, index: usize) -> Option<PaneMemory> {
        self.tabs.remove(index);
        if index == self.selected {
            self.selected = self.selected.min(self.tabs.len() - 1);
            Some(std::mem::take(self.tabs[self.selected].active()))
        } else {
            if index < self.selected {
                self.selected -= 1;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_layout_keeps_both_lists_readable_or_shows_one_side_at_a_time() {
        assert!(split_widths(575.0, 0.5).is_none());
        assert_eq!(split_widths(800.0, 0.5), Some([396.0, 396.0]));
        let pair = split_widths(800.0, 0.95).unwrap();
        assert!(pair.iter().all(|width| *width >= 320.0));
        assert_eq!(pair[0] + pair[1], 792.0);
    }
}
