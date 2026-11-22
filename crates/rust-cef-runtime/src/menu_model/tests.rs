//! Reader fixtures exercise the actual copier algorithm, not CEF native calls.
use super::*;
use std::{cell::Cell, num::NonZeroUsize, rc::Rc};
#[derive(Clone)]
struct Row {
    kind: Kind,
    id: i32,
    text: Vec<u16>,
    visible: bool,
    enabled: bool,
    checked: bool,
    group: i32,
    child: Option<Source>,
}
#[derive(Clone)]
struct Source {
    rows: Vec<Row>,
    reported: Option<usize>,
    churn: bool,
    count_calls: Cell<usize>,
    labels: Rc<Cell<usize>>,
}
fn source(rows: Vec<Row>) -> Source {
    Source {
        rows,
        reported: None,
        churn: false,
        count_calls: Cell::new(0),
        labels: Rc::new(Cell::new(0)),
    }
}
fn row(kind: Kind, id: i32, text: &str) -> Row {
    Row {
        kind,
        id,
        text: text.encode_utf16().collect(),
        visible: true,
        enabled: true,
        checked: false,
        group: 7,
        child: None,
    }
}
impl Reader for Source {
    fn count(&self) -> usize {
        let n = self.count_calls.get();
        self.count_calls.set(n + 1);
        self.reported.unwrap_or(self.rows.len()) + usize::from(self.churn && n > 0)
    }
    fn kind(&self, i: usize) -> Kind {
        self.rows[i].kind
    }
    fn visible(&self, i: usize) -> bool {
        self.rows[i].visible
    }
    fn enabled(&self, i: usize) -> bool {
        self.rows[i].enabled
    }
    fn command_id(&self, i: usize) -> i32 {
        self.rows[i].id
    }
    fn checked(&self, i: usize) -> bool {
        self.rows[i].checked
    }
    fn group_id(&self, i: usize) -> i32 {
        self.rows[i].group
    }
    fn label(&self, i: usize, limit: usize) -> Result<String, CopyError> {
        self.labels.set(self.labels.get() + 1);
        copy_utf16(&self.rows[i].text, limit)
    }
    fn submenu(&self, i: usize) -> Option<Self> {
        self.rows[i].child.clone()
    }
}
fn limits(rows: usize, label: usize, total: usize) -> MenuLimits {
    MenuLimits {
        max_entries: NonZeroUsize::new(rows).unwrap(),
        max_label_bytes: NonZeroUsize::new(label).unwrap(),
        max_total_label_bytes: NonZeroUsize::new(total).unwrap(),
    }
}
#[test]
fn owned_copy_preserves_all_states_raw_separators_and_parent_local_radio_groups() {
    let mut hidden = row(Kind::Check, 2, "Check");
    hidden.visible = false;
    let mut radio = row(Kind::Radio, 3, "R");
    radio.checked = true;
    let mut sep = row(Kind::Separator, -1, "");
    sep.visible = false;
    let mut group = row(Kind::Submenu, 99, "G");
    group.visible = false;
    group.child = Some(source(vec![
        sep,
        row(Kind::Radio, 4, "r"),
        row(Kind::Separator, -1, ""),
    ]));
    let src = source(vec![
        row(Kind::Separator, -1, ""),
        row(Kind::Command, 1, "中"),
        hidden,
        radio,
        group,
        row(Kind::Separator, -1, ""),
    ]);
    let m = copy_root(&src, 12, -4, limits(9, 8, 11)).unwrap();
    assert_eq!((m.x, m.y, m.entries.len()), (12, -4, 6)); // no pre-admission normalization
    let MenuItem::Command(c) = &m.entries[2] else {
        panic!("check lost")
    };
    assert_eq!(
        (c.visible, c.checked, c.radio_group),
        (false, Some(false), None)
    );
    let MenuItem::Command(c) = &m.entries[3] else {
        panic!("radio lost")
    };
    assert_eq!((c.checked, c.radio_group), (Some(true), Some(7)));
    let MenuItem::Submenu(g) = &m.entries[4] else {
        panic!("child lost")
    };
    assert!(!g.visible);
    assert_eq!(g.entries.len(), 3);
    assert!(matches!(
        g.entries[0],
        MenuChild::Separator { visible: false }
    ));
    let MenuChild::Command(c) = &g.entries[1] else {
        panic!("child radio lost")
    };
    assert_eq!(c.radio_group, Some(7));
    assert!(m.allows_selection(1));
    assert!(m.allows_selection(3));
    assert!(!m.allows_selection(2));
    assert!(!m.allows_selection(4));
    assert_eq!(src.labels.get(), 4);
    assert_eq!(src.rows[4].child.as_ref().unwrap().labels.get(), 1);
    assert_eq!(
        copy_root(&src, 0, 0, limits(9, 8, 10)),
        Err(CopyError::LabelBudget)
    );
    assert_eq!(
        copy_root(&src, 0, 0, limits(9, 2, 11)),
        Err(CopyError::LabelBudget)
    );
}
#[test]
fn copier_refuses_raw_overflow_deep_missing_unknown_churn_and_invalid_models() {
    let mut huge = source(vec![]);
    huge.reported = Some(usize::MAX);
    assert_eq!(
        copy_root(&huge, 0, 0, limits(8, 32, 64)),
        Err(CopyError::RowBudget)
    );
    assert_eq!(huge.labels.get(), 0);
    let mut g = row(Kind::Submenu, 10, "G");
    g.child = Some(source(vec![
        row(Kind::Command, 1, "A"),
        row(Kind::Command, 2, "B"),
    ]));
    let src = source(vec![g.clone()]);
    let child_labels = src.rows[0].child.as_ref().unwrap().labels.clone();
    assert_eq!(
        copy_root(&src, 0, 0, limits(2, 32, 64)),
        Err(CopyError::RowBudget)
    );
    assert_eq!(child_labels.get(), 0);
    g.child = None;
    assert_eq!(
        copy_root(&source(vec![g.clone()]), 0, 0, limits(8, 32, 64)),
        Err(CopyError::MissingSubmenu)
    );
    g.visible = false;
    g.child = Some(source(vec![row(Kind::Submenu, 11, "Deep")]));
    assert_eq!(
        copy_root(&source(vec![g.clone()]), 0, 0, limits(8, 32, 64)),
        Err(CopyError::TooDeep)
    );
    for k in [Kind::Unsupported(0), Kind::Unsupported(999)] {
        let mut r = row(k, 1, "X");
        r.visible = false;
        assert_eq!(
            copy_root(&source(vec![r]), 0, 0, limits(8, 32, 64)),
            Err(CopyError::UnsupportedType(if k == Kind::Unsupported(0) {
                0
            } else {
                999
            }))
        );
    }
    let mut changed = source(vec![row(Kind::Command, 1, "A")]);
    changed.churn = true;
    assert_eq!(
        copy_root(&changed, 0, 0, limits(8, 32, 64)),
        Err(CopyError::ChangedDuringCopy)
    );
    let cases = [
        (vec![], MenuTreeError::Empty),
        (
            vec![row(Kind::Command, 0, "A")],
            MenuTreeError::InvalidCommandId,
        ),
        (vec![row(Kind::Command, 1, "")], MenuTreeError::EmptyLabel),
        (
            vec![row(Kind::Command, 1, "A"), row(Kind::Command, 1, "B")],
            MenuTreeError::DuplicateCommandId,
        ),
    ];
    for (rows, error) in cases {
        assert_eq!(
            copy_root(&source(rows), 0, 0, limits(8, 32, 64)),
            Err(CopyError::Model(error))
        );
    }
    g.visible = true;
    g.child = Some(source(vec![row(Kind::Separator, -1, "")]));
    assert_eq!(
        copy_root(&source(vec![g]), 0, 0, limits(8, 32, 64)),
        Err(CopyError::Model(MenuTreeError::EmptySubmenu))
    );
}
#[test]
fn utf16_preflight_checks_exact_utf8_bytes_and_rejects_invalid_surrogates() {
    let text: Vec<u16> = "中🙂".encode_utf16().collect();
    assert_eq!(copy_utf16(&text, 7).unwrap(), "中🙂");
    assert_eq!(copy_utf16(&text, 6), Err(CopyError::LabelBudget));
    assert_eq!(copy_utf16(&[0xD800], 8), Err(CopyError::InvalidUtf16));
    assert_eq!(copy_utf16(&[0xDC00], 8), Err(CopyError::InvalidUtf16));
    assert_eq!(copy_utf16(&[65, 66, 67], 2), Err(CopyError::LabelBudget));
    assert_eq!(copy_utf16(&[], 1), Ok(String::new()));
}
