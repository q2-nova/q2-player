//! Bounded continuous content, independent of request completion order.
//! Pages stay an implementation detail; a screen retains its selected row key.
use super::list::Row;
use std::collections::{BTreeMap, HashSet};

const RETAINED_PAGES: usize = 5;
const EDGE_ROWS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Request { pub page: u32, pub token: u64, new_page: bool, jump: bool }
struct Chunk { rows: Vec<Row>, more: bool }
#[derive(Default)]
pub(super) struct Window {
    generation: u64,
    pending: Option<Request>,
    chunks: BTreeMap<u32, Chunk>,
    failed: Option<u32>,
    visible_len: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied { Stale, Content, NoProgress }
impl Window {
    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None; self.failed = None; self.chunks.clear(); self.visible_len = 0;
    }
    pub fn begin(&mut self, page: u32) -> Option<Request> {
        if self.pending.is_some() { return None; }
        if let (Some((&first,_)),Some((&last,_)))=(self.chunks.first_key_value(),self.chunks.last_key_value()) {
            if page<first.saturating_sub(1) || page>last.saturating_add(1) {return None;}
        }
        self.generation = self.generation.wrapping_add(1);
        let request = Request {page, token:self.generation, new_page:!self.chunks.contains_key(&page), jump:false};
        self.pending = Some(request);
        Some(request)
    }
    /// Admit an indexed jump without discarding visible content on failure.
    pub fn begin_jump(&mut self, page: u32) -> Request {
        self.generation = self.generation.wrapping_add(1);
        let request = Request { page, token: self.generation, new_page: true, jump: true };
        self.pending = Some(request);
        request
    }
    pub fn position_for_key(&self, key: &str, page_size: u32) -> Option<u32> {
        self.chunks.iter().find_map(|(&page, chunk)| {
            chunk.rows.iter().position(|row| row.key == key)
                .and_then(|index| page.checked_mul(page_size)?.checked_add(index as u32))
        })
    }
    pub fn pending(&self) -> Option<Request> { self.pending }
    pub fn page_for_key(&self, key: &str) -> Option<u32> {
        self.chunks.iter().find(|(_, chunk)| chunk.rows.iter().any(|row| row.key == key))
            .map(|(&page, _)| page)
    }
    pub fn failure_before_content(&self) -> bool {
        self.failed.zip(self.chunks.first_key_value().map(|(&page, _)| page))
            .is_some_and(|(failed, first)| failed < first)
    }
    pub fn failed(&self) -> Option<u32> { self.failed }
    pub fn retry_visible(&self) -> bool {
        let Some(page)=self.failed else {return false;};
        let (Some((&first,_)),Some((&last,_)))=(self.chunks.first_key_value(),self.chunks.last_key_value()) else {return true;};
        page>=first.saturating_sub(1) && page<=last.saturating_add(1)
    }
    pub fn fail(&mut self, token: u64) -> bool {
        let Some(request) = self.pending.filter(|request|request.token==token) else { return false; };
        self.pending = None; self.failed = Some(request.page); true
    }
    /// A cache snapshot can arrive before the final refresh. It never admits
    /// another request until that refresh (or its failure) has completed.
    pub fn apply(&mut self, token: u64, rows: Vec<Row>, more: bool,
                 final_reply: bool, anchor: Option<&str>) -> Applied {
        let Some(request) = self.pending.filter(|request|request.token==token) else { return Applied::Stale; };
        if request.jump { self.chunks.clear(); self.failed = None; }
        let anchor = if request.jump { None } else { anchor };
        let other_keys: HashSet<_> = self.chunks.iter().filter(|(page,_)|**page!=request.page)
            .flat_map(|(_,chunk)|chunk.rows.iter().map(|row|row.key.as_str())).collect();
        if final_reply && request.new_page && !rows.is_empty()
            && rows.iter().all(|row|other_keys.contains(row.key.as_str())) {
            // Repeated backend pages are not proof of exhaustion. Stop the
            // automatic request loop and leave an explicit retryable failure.
            self.chunks.remove(&request.page);
            self.recount();
            self.pending = None; self.failed = Some(request.page);
            return Applied::NoProgress;
        }
        let more = more && !rows.is_empty();
        self.chunks.insert(request.page, Chunk {rows, more});
        let anchor_page = anchor.and_then(|key|self.chunks.iter()
            .find(|(_,chunk)|chunk.rows.iter().any(|row|row.key==key)).map(|(&page,_)|page))
            .unwrap_or(request.page);
        while self.chunks.len()>RETAINED_PAGES {
            let page = *self.chunks.keys().max_by_key(|page|page.abs_diff(anchor_page)).unwrap();
            self.chunks.remove(&page);
        }
        self.recount();
        if final_reply {
            self.pending = None;
            if self.failed==Some(request.page) {self.failed=None;}
        }
        Applied::Content
    }
    fn recount(&mut self) {
        let mut keys=HashSet::new();
        self.visible_len=self.chunks.values().flat_map(|chunk|chunk.rows.iter())
            .filter(|row|keys.insert(row.key.as_str())).count();
    }
    pub fn rows(&self) -> Vec<Row> {
        let mut keys=HashSet::new();
        self.chunks.values().flat_map(|chunk|chunk.rows.iter())
            .filter(|row|keys.insert(row.key.as_str())).cloned().collect()
    }
    /// Triggered by browsing toward an edge, never by rendering a frame.
    pub fn next(&self, selected: usize, direction: i32) -> Option<u32> {
        if self.pending.is_some() { return None; }
        let (&first,_) = self.chunks.first_key_value()?;
        let (&last,chunk) = self.chunks.last_key_value()?;
        if direction<0 && selected<EDGE_ROWS && first>0 { return (self.failed!=Some(first-1)).then_some(first-1); }
        if direction>0 && chunk.more && selected.saturating_add(EDGE_ROWS)>=self.visible_len {
            return last.checked_add(1).filter(|page|self.failed!=Some(*page));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::list::RowAction;
    fn rows(page:u32)->Vec<Row> {(0..50).map(|i|Row::new(format!("Generated {}",page*50+i),RowAction::Cmd(1)).key(format!("{}",page*50+i))).collect()}
    #[test]
    fn indexed_jumps_keep_failed_context_and_reject_late_adjacent_pages() {
        let mut window=Window::default(); let first=window.begin(0).unwrap();
        window.apply(first.token,rows(0),true,true,None);
        let adjacent=window.begin(1).unwrap(); let jump=window.begin_jump(900);
        assert_eq!(window.apply(adjacent.token,rows(1),true,true,None),Applied::Stale);
        assert!(window.fail(jump.token)); assert_eq!(window.rows()[0].key,"0");
        let jump=window.begin_jump(900);
        assert_eq!(window.apply(jump.token,rows(900),true,true,Some("0")),Applied::Content);
        assert_eq!(window.rows().len(),50); assert_eq!(window.rows()[0].key,"45000");
        assert_eq!(window.position_for_key("45012",50),Some(45012));
        assert_eq!(window.next(0,-1),Some(899));
    }
    #[test]
    fn million_item_browsing_retains_selection_with_bounded_forward_and_reverse_windows() {
        let mut window=Window::default();
        let mut anchor=None;
        for page in 0..20_000 {
            let request=window.begin(page).unwrap();
            assert_eq!(window.apply(request.token,rows(page),page<19_999,true,anchor.as_deref()),Applied::Content);
            let shown=window.rows();
            assert!(shown.len()<=250);
            if let Some(key)=&anchor {assert!(shown.iter().any(|row|&row.key==key));}
            anchor=Some(format!("{}",page*50+49));
            assert_eq!(window.next(shown.len()-1,1),if page<19_999 {Some(page+1)} else {None});
        }
        for page in (0..19_995).rev() {
            let shown=window.rows();let key=shown[0].key.clone();
            let request=window.begin(page).unwrap();
            window.apply(request.token,rows(page),true,true,Some(&key));
            let shown=window.rows();assert!(shown.len()<=250);assert!(shown.iter().any(|row|row.key==key));
            assert_eq!(window.next(0,-1),page.checked_sub(1));
        }
    }
    #[test]
    fn cache_refresh_slow_failure_retry_and_stale_account_completions_are_ordered() {
        let mut window=Window::default();let first=window.begin(0).unwrap();
        window.apply(first.token,rows(0),true,false,None);
        assert!(window.begin(1).is_none());assert_eq!(window.next(49,1),None);
        window.apply(first.token,rows(0),true,true,None);
        let next=window.begin(1).unwrap();let before:Vec<_>=window.rows().iter().map(|row|row.key.clone()).collect();
        assert!(window.fail(next.token));assert_eq!(window.failed(),Some(1));assert_eq!(window.next(49,1),None);
        assert_eq!(window.rows().iter().map(|row|row.key.clone()).collect::<Vec<_>>(),before);
        let retry=window.begin(1).unwrap();assert_eq!(window.apply(next.token,rows(9),true,true,None),Applied::Stale);
        window.apply(retry.token,rows(1),true,true,Some("49"));assert_eq!(window.rows().len(),100);
        let stale=window.begin(2).unwrap();window.reset();
        assert_eq!(window.apply(stale.token,rows(2),true,true,None),Applied::Stale);assert!(!window.fail(stale.token));assert!(window.rows().is_empty());
    }
    #[test]
    fn a_failed_forward_page_does_not_block_cached_history_in_the_other_direction() {
        let mut window=Window::default();
        for page in 2..7 {
            let request=window.begin(page).unwrap();window.apply(request.token,rows(page),true,true,None);
        }
        let request=window.begin(7).unwrap();window.fail(request.token);
        assert_eq!(window.next(249,1),None);assert_eq!(window.next(0,-1),Some(1));
        let history=window.begin(1).unwrap();window.apply(history.token,rows(1),true,true,Some("100"));
        assert_eq!(window.failed(),Some(7));assert!(window.rows().iter().any(|row|row.key=="50"));
    }
    #[test]
    fn returning_through_history_cannot_retry_across_a_gap_in_the_retained_window() {
        let mut window=Window::default();
        for page in 4..9 {
            let request=window.begin(page).unwrap();window.apply(request.token,rows(page),true,true,None);
        }
        let request=window.begin(9).unwrap();window.fail(request.token);
        assert!(window.retry_visible());
        for page in (0..4).rev() {
            let key=window.rows()[0].key.clone();let request=window.begin(page).unwrap();
            window.apply(request.token,rows(page),true,true,Some(&key));
        }
        assert!(!window.retry_visible());assert!(window.begin(9).is_none());
        for page in 5..9 {
            assert_eq!(window.next(window.visible_len-1,1),Some(page));
            let key=window.rows().last().unwrap().key.clone();let request=window.begin(page).unwrap();
            window.apply(request.token,rows(page),true,true,Some(&key));
        }
        assert!(window.retry_visible());assert_eq!(window.next(window.visible_len-1,1),None);
        let request=window.begin(9).unwrap();window.apply(request.token,rows(9),false,true,Some("449"));
        let shown=window.rows();assert!(shown.windows(2).all(|pair|pair[1].key.parse::<u32>().unwrap()==pair[0].key.parse::<u32>().unwrap()+1));
    }
    #[test]
    fn empty_terminal_and_duplicate_backend_pages_cannot_spin_or_invent_rows() {
        let mut window=Window::default();let request=window.begin(0).unwrap();
        window.apply(request.token,Vec::new(),true,true,None);assert!(window.rows().is_empty());assert_eq!(window.next(0,1),None);
        window.reset();let request=window.begin(0).unwrap();window.apply(request.token,rows(0),true,true,None);
        let repeat=window.begin(1).unwrap();window.apply(repeat.token,rows(0),true,false,Some("49"));
        assert_eq!(window.apply(repeat.token,rows(0),true,true,Some("49")),Applied::NoProgress);
        assert_eq!(window.failed(),Some(1));assert_eq!(window.rows().len(),50);assert_eq!(window.next(49,1),None);
        let retry=window.begin(1).unwrap();window.apply(retry.token,rows(1),false,true,Some("49"));
        assert_eq!(window.rows().len(),100);assert_eq!(window.next(99,1),None);
        window.reset();
        for page in 0..5 {
            let request=window.begin(page).unwrap();window.apply(request.token,rows(page),true,true,None);
        }
        let request=window.begin(5).unwrap();
        window.apply(request.token,rows(5),true,false,Some("249"));
        assert_eq!(window.apply(request.token,rows(4),true,true,Some("249")),Applied::NoProgress);
        assert_eq!(window.rows().len(),200);
        assert_eq!(window.visible_len,window.rows().len(),"rejected fresh page must undo the cached window count");
    }
}
