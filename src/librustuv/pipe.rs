// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::c_str::CString;
use std::cast;
use std::libc;
use std::rt::BlockedTask;
use std::rt::io::IoError;
use std::rt::local::Local;
use std::rt::rtio::{RtioPipe, RtioUnixListener, RtioUnixAcceptor};
use std::rt::sched::{Scheduler, SchedHandle};
use std::rt::tube::Tube;

use stream::StreamWatcher;
use super::{Loop, UvError, NativeHandle, uv_error_to_io_error, UvHandle};
use uvio::HomingIO;
use uvll;

pub struct PipeWatcher {
    stream: StreamWatcher,
    home: SchedHandle,
}

pub struct PipeListener {
    home: SchedHandle,
    pipe: *uvll::uv_pipe_t,
    priv closing_task: Option<BlockedTask>,
    priv outgoing: Tube<Result<~RtioPipe, IoError>>,
}

pub struct PipeAcceptor {
    listener: ~PipeListener,
    priv incoming: Tube<Result<~RtioPipe, IoError>>,
}

// PipeWatcher implementation and traits

impl PipeWatcher {
    pub fn new(pipe: *uvll::uv_pipe_t) -> PipeWatcher {
        PipeWatcher {
            stream: StreamWatcher::new(pipe),
            home: get_handle_to_current_scheduler!(),
        }
    }

    pub fn alloc(loop_: &Loop, ipc: bool) -> *uvll::uv_pipe_t {
        unsafe {
            let handle = uvll::malloc_handle(uvll::UV_NAMED_PIPE);
            assert!(!handle.is_null());
            let ipc = ipc as libc::c_int;
            assert_eq!(uvll::uv_pipe_init(loop_.native_handle(), handle, ipc), 0);
            handle
        }
    }

    pub fn open(loop_: &Loop, file: libc::c_int) -> Result<PipeWatcher, UvError>
    {
        let handle = PipeWatcher::alloc(loop_, false);
        match unsafe { uvll::uv_pipe_open(handle, file) } {
            0 => Ok(PipeWatcher::new(handle)),
            n => {
                unsafe { uvll::uv_close(handle, pipe_close_cb) }
                Err(UvError(n))
            }
        }
    }

    pub fn connect(loop_: &Loop, name: &CString) -> Result<PipeWatcher, UvError>
    {
        struct Ctx {
            task: Option<BlockedTask>,
            result: Option<Result<PipeWatcher, UvError>>,
        }
        let mut cx = Ctx { task: None, result: None };
        let req = unsafe { uvll::malloc_req(uvll::UV_CONNECT) };
        unsafe { uvll::set_data_for_req(req, &cx as *Ctx) }

        let sched: ~Scheduler = Local::take();
        do sched.deschedule_running_task_and_then |_, task| {
            cx.task = Some(task);
            unsafe {
                uvll::uv_pipe_connect(req,
                                      PipeWatcher::alloc(loop_, false),
                                      name.with_ref(|p| p),
                                      connect_cb)
            }
        }
        assert!(cx.task.is_none());
        return cx.result.take().expect("pipe connect needs a result");

        extern fn connect_cb(req: *uvll::uv_connect_t, status: libc::c_int) {
            unsafe {
                let cx: &mut Ctx = cast::transmute(uvll::get_data_for_req(req));
                let stream = uvll::get_stream_handle_from_connect_req(req);
                cx.result = Some(match status {
                    0 => Ok(PipeWatcher::new(stream)),
                    n => {
                        uvll::free_handle(stream);
                        Err(UvError(n))
                    }
                });
                uvll::free_req(req);

                let sched: ~Scheduler = Local::take();
                sched.resume_blocked_task_immediately(cx.task.take_unwrap());
            }
        }
    }
}

impl RtioPipe for PipeWatcher {
    fn read(&mut self, buf: &mut [u8]) -> Result<uint, IoError> {
        let _m = self.fire_missiles();
        self.stream.read(buf).map_err(uv_error_to_io_error)
    }

    fn write(&mut self, buf: &[u8]) -> Result<(), IoError> {
        let _m = self.fire_missiles();
        self.stream.write(buf).map_err(uv_error_to_io_error)
    }
}

impl HomingIO for PipeWatcher {
    fn home<'a>(&'a mut self) -> &'a mut SchedHandle { &mut self.home }
}

impl Drop for PipeWatcher {
    fn drop(&mut self) {
        let _m = self.fire_missiles();
        self.stream.close(true); // close synchronously
    }
}

extern fn pipe_close_cb(handle: *uvll::uv_handle_t) {
    unsafe { uvll::free_handle(handle) }
}

// PipeListener implementation and traits

impl PipeListener {
    pub fn bind(loop_: &Loop, name: &CString) -> Result<~PipeListener, UvError> {
        let pipe = PipeWatcher::alloc(loop_, false);
        match unsafe { uvll::uv_pipe_bind(pipe, name.with_ref(|p| p)) } {
            0 => {
                let p = ~PipeListener {
                    home: get_handle_to_current_scheduler!(),
                    pipe: pipe,
                    closing_task: None,
                    outgoing: Tube::new(),
                };
                Ok(p.install())
            }
            n => {
                unsafe { uvll::uv_close(pipe, pipe_close_cb) }
                Err(UvError(n))
            }
        }
    }
}

impl RtioUnixListener for PipeListener {
    fn listen(mut ~self) -> Result<~RtioUnixAcceptor, IoError> {
        // create the acceptor object from ourselves
        let incoming = self.outgoing.clone();
        let mut acceptor = ~PipeAcceptor {
            listener: self,
            incoming: incoming,
        };

        let _m = acceptor.fire_missiles();
        // XXX: the 128 backlog should be configurable
        match unsafe { uvll::uv_listen(acceptor.listener.pipe, 128, listen_cb) } {
            0 => Ok(acceptor as ~RtioUnixAcceptor),
            n => Err(uv_error_to_io_error(UvError(n))),
        }
    }
}

impl HomingIO for PipeListener {
    fn home<'r>(&'r mut self) -> &'r mut SchedHandle { &mut self.home }
}

impl UvHandle<uvll::uv_pipe_t> for PipeListener {
    fn uv_handle(&self) -> *uvll::uv_pipe_t { self.pipe }
}

extern fn listen_cb(server: *uvll::uv_stream_t, status: libc::c_int) {
    let msg = match status {
        0 => {
            let loop_ = NativeHandle::from_native_handle(unsafe {
                uvll::get_loop_for_uv_handle(server)
            });
            let client = PipeWatcher::alloc(&loop_, false);
            assert_eq!(unsafe { uvll::uv_accept(server, client) }, 0);
            Ok(~PipeWatcher::new(client) as ~RtioPipe)
        }
        n => Err(uv_error_to_io_error(UvError(n)))
    };

    let pipe: &mut PipeListener = unsafe { UvHandle::from_uv_handle(&server) };
    pipe.outgoing.send(msg);
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        let (_m, sched) = self.fire_missiles_sched();

        do sched.deschedule_running_task_and_then |_, task| {
            self.closing_task = Some(task);
            unsafe { uvll::uv_close(self.pipe, listener_close_cb) }
        }
    }
}

extern fn listener_close_cb(handle: *uvll::uv_handle_t) {
    let pipe: &mut PipeListener = unsafe { UvHandle::from_uv_handle(&handle) };
    unsafe { uvll::free_handle(handle) }

    let sched: ~Scheduler = Local::take();
    sched.resume_blocked_task_immediately(pipe.closing_task.take_unwrap());
}

// PipeAcceptor implementation and traits

impl RtioUnixAcceptor for PipeAcceptor {
    fn accept(&mut self) -> Result<~RtioPipe, IoError> {
        let _m = self.fire_missiles();
        self.incoming.recv()
    }
}

impl HomingIO for PipeAcceptor {
    fn home<'r>(&'r mut self) -> &'r mut SchedHandle { self.listener.home() }
}
