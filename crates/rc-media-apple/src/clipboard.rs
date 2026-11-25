//! An explicit text exchange. Only the iOS paste button reads the system
//! pasteboard; the network thread never reads a user's clipboard on its own.
use rc_session::ClipboardBackend;
use rc_types::Result;
use std::ffi::{c_char, CStr};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Default)]
struct Board {
    ready: bool,
    count: u64,
    outgoing: Option<String>,
    received: Option<String>,
}

type Slot = Arc<Mutex<Board>>;
fn boards() -> &'static Mutex<Vec<Weak<Mutex<Board>>>> {
    static ACTIVE: OnceLock<Mutex<Vec<Weak<Mutex<Board>>>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

fn only_board() -> Option<Slot> {
    let boards = boards().lock().ok()?;
    let mut active = boards.iter().filter_map(Weak::upgrade);
    let first = active.next()?;
    active.next().is_none().then_some(first)
}

struct Backend(Slot);
impl ClipboardBackend for Backend {
    fn change_count(&mut self) -> Result<u64> {
        let mut board = self.0.lock().unwrap();
        board.ready = true;
        Ok(board.count)
    }
    fn read_text(&mut self) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().outgoing.clone())
    }
    fn write_text(&mut self, text: &str) -> Result<()> {
        self.0.lock().unwrap().received = Some(text.to_owned());
        Ok(())
    }
}

#[cfg(target_os = "ios")]
pub fn register() {
    rc_session::register_clipboard_provider(Arc::new(|| {
        let board = Slot::default();
        let mut boards = boards().lock().unwrap();
        boards.retain(|w| w.strong_count() > 0);
        boards.push(Arc::downgrade(&board));
        Ok(Box::new(Backend(board)))
    }));
}

/// Detach previous workers and discard pending text at the session boundary.
#[no_mangle]
pub extern "C" fn rc_apple_clipboard_reset() {
    for slot in boards().lock().unwrap().iter().filter_map(Weak::upgrade) {
        *slot.lock().unwrap() = Board::default();
    }
}

/// UTF-8 encoded JSON string. Does not read the system clipboard.
#[no_mangle]
pub unsafe extern "C" fn rc_apple_clipboard_send(json: *const c_char) -> i32 {
    if json.is_null() {
        return 0;
    }
    let Ok(text) = serde_json::from_slice::<String>(unsafe { CStr::from_ptr(json) }.to_bytes())
    else {
        return 0;
    };
    if text.len() > u16::MAX as usize {
        return 0;
    }
    let Some(slot) = only_board() else { return 0 };
    let mut board = slot.lock().unwrap();
    if !board.ready {
        return 0;
    }
    board.count = board.count.wrapping_add(1);
    board.outgoing = Some(text);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_text_neither_echoes_nor_overwrites_a_pending_send() {
        let mut backend = Backend(Slot::default());
        backend.0.lock().unwrap().outgoing = Some("local".into());
        let before = backend.change_count().unwrap();
        backend.write_text("remote").unwrap();
        assert_eq!(backend.change_count().unwrap(), before);
        assert_eq!(backend.read_text().unwrap().as_deref(), Some("local"));
        assert_eq!(
            backend.0.lock().unwrap().received.as_deref(),
            Some("remote")
        );
    }
}

#[no_mangle]
pub extern "C" fn rc_apple_clipboard_take() -> *mut c_char {
    let Some(slot) = only_board() else {
        return std::ptr::null_mut();
    };
    let text = slot.lock().unwrap().received.take();
    text.map(|v| super::to_c_string(serde_json::to_string(&v).unwrap()))
        .unwrap_or(std::ptr::null_mut())
}
