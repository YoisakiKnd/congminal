#[cfg(test)]
mod allocation_native_input_probe {
    use super::*;
    use crate::auth_allocation_meter as meter;
    #[test]
    fn measured_input_native_conversion_and_discard_scrub() {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let exact = "x".repeat(1020) + "文";
            let admitted = nsstring(&exact);
            let oversized = nsstring(&"x".repeat(super::super::CLIPBOARD_LIMIT + 1));
            // 提前完成 ObjC 方法解析和线程局部队列/日志状态初始化。
            drop(string_from(admitted, 1023));
            assert!(string_from(oversized, 1023).is_none());
            crate::term::trace(format_args!("input allocation fixture initialized"));
            IME.with(|state| {
                let mut state = state.borrow_mut();
                state.events.reserve(2);
                state.events.clear();
                state.in_key_down = true;
                state.composing_at_start = false;
                assert!(state.pending_text.is_none());
            });
            meter::start();
            let text = string_from(admitted, 1023).unwrap();
            assert_eq!(text, exact);
            let first = meter::peek();
            assert_eq!((first.allocations, first.reallocations, first.peak), (1,0,1023));
            meter::watch_clear(text.as_bytes());
            IME.with(|state| state.borrow_mut().pending_text = Some(text));
            // 合法后继回调替换前一暂存，必须在释放前清零。
            insert_text(NIL, std::ptr::null(), admitted, NSRange{location:0,length:0});
            IME.with(|state| meter::watch_clear(state.borrow().pending_text.as_ref().unwrap().as_bytes()));
            // 超限回调不创建 String；也不能回落为 Key 或保留前一密码。
            let before = meter::peek();
            insert_text(NIL, std::ptr::null(), oversized, NSRange{location:0,length:0});
            let after = meter::peek();
            assert_eq!(before.allocations, after.allocations);
            IME.with(|state| {
                let mut state = state.borrow_mut();
                assert!(state.pending_text.is_none());
                assert!(state.consumed);
                assert!(matches!(state.events.pop_front(), Some(Event::Preedit(text)) if text.is_empty()));
                assert!(state.events.is_empty());
                state.in_key_down = false;
            });
            let counts = meter::stop();
            assert_eq!((counts.allocations, counts.reallocations, counts.live, counts.peak), (2,0,0,1023));
            assert_eq!((counts.scrub_checks, counts.scrub_failures), (2,0));
            eprintln!("INPUT_NATIVE allocations={} reallocations={} peak={} final_live={} scrub_checks={} scrub_failures={}", counts.allocations, counts.reallocations, counts.peak, counts.live, counts.scrub_checks, counts.scrub_failures);
            objc_autoreleasePoolPop(pool);
        }
    }
}
