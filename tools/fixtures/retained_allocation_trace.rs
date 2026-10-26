//! Diagnostic only: retained 48-byte requests, fixed table and native stacks, never a timing probe.
use std::{cell::UnsafeCell, ffi::c_void, sync::atomic::{AtomicBool, Ordering}};
unsafe extern "C" { fn backtrace(frames:*mut *mut c_void,count:i32)->i32;fn dladdr(address:*const c_void,info:*mut DlInfo)->i32; }
#[repr(C)]struct DlInfo {file:*const u8,base:*mut c_void,symbol:*const u8,address:*mut c_void}
#[derive(Clone,Copy)]struct Trace{pointer:usize,length:usize,frames:[usize;32]}
const EMPTY:Trace=Trace{pointer:0,length:0,frames:[0;32]};
struct Table(UnsafeCell<[Trace;256]>);unsafe impl Sync for Table{}
static TABLE:Table=Table(UnsafeCell::new([EMPTY;256]));static LOCK:AtomicBool=AtomicBool::new(false);static ENABLED:AtomicBool=AtomicBool::new(false);static OVERFLOW:AtomicBool=AtomicBool::new(false);
fn locked(f:impl FnOnce(&mut [Trace;256])){while LOCK.compare_exchange_weak(false,true,Ordering::Acquire,Ordering::Relaxed).is_err(){std::hint::spin_loop();}f(unsafe{&mut *TABLE.0.get()});LOCK.store(false,Ordering::Release);}
pub fn begin(){let mut frames=[std::ptr::null_mut();32];unsafe{backtrace(frames.as_mut_ptr(),32);}ENABLED.store(true,Ordering::SeqCst);}
pub fn add(pointer:*mut u8,size:usize){if size!=48 || !ENABLED.load(Ordering::SeqCst){return;}let mut frames=[std::ptr::null_mut();32];let length=unsafe{backtrace(frames.as_mut_ptr(),32)}.max(0) as usize;locked(|table|{if !ENABLED.load(Ordering::SeqCst){return;}if let Some(slot)=table.iter_mut().find(|t|t.pointer==0){*slot=Trace{pointer:pointer as usize,length,frames:frames.map(|p|p as usize)};}else{OVERFLOW.store(true,Ordering::Relaxed);}});}
pub fn remove(pointer:*mut u8){if !ENABLED.load(Ordering::SeqCst){return;}locked(|table|{if let Some(t)=table.iter_mut().find(|t|t.pointer==pointer as usize){*t=EMPTY;}});}
pub fn end(){ENABLED.store(false,Ordering::SeqCst);let mut info=DlInfo{file:std::ptr::null(),base:std::ptr::null_mut(),symbol:std::ptr::null(),address:std::ptr::null_mut()};unsafe{dladdr(end as *const c_void,&mut info);}eprintln!("RETAINED_BASE base={:#x} overflow={}",info.base as usize,OVERFLOW.load(Ordering::Relaxed));let mut table=[EMPTY;256];locked(|live|table=*live);for t in table.iter().filter(|t|t.pointer!=0){eprint!("RETAINED_EVENT pointer={:#x} frames=",t.pointer);for f in &t.frames[..t.length]{eprint!("{f:#x},");}eprintln!();}}
