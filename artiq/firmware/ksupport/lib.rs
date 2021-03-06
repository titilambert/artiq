#![feature(lang_items, asm, libc, panic_unwind, unwind_attributes)]
#![no_std]

extern crate unwind;
extern crate libc;
extern crate byteorder;
extern crate cslice;

extern crate alloc_none;
extern crate std_artiq as std;

extern crate board;
extern crate dyld;
extern crate proto;
extern crate amp;

use core::{mem, ptr, slice, str};
use std::io::Cursor;
use cslice::{CSlice, AsCSlice};
use board::csr;
use dyld::Library;
use proto::{kernel_proto, rpc_proto};
use proto::kernel_proto::*;
use amp::{mailbox, rpc_queue};

fn send(request: &Message) {
    unsafe { mailbox::send(request as *const _ as usize) }
    while !mailbox::acknowledged() {}
}

fn recv<R, F: FnOnce(&Message) -> R>(f: F) -> R {
    while mailbox::receive() == 0 {}
    let result = f(unsafe { mem::transmute::<usize, &Message>(mailbox::receive()) });
    mailbox::acknowledge();
    result
}

macro_rules! recv {
    ($p:pat => $e:expr) => {
        recv(move |request| {
            if let $p = request {
                $e
            } else {
                send(&Log(format_args!("unexpected reply: {:?}\n", request)));
                loop {}
            }
        })
    }
}

#[no_mangle]
#[lang = "panic_fmt"]
pub extern fn panic_fmt(args: core::fmt::Arguments, file: &'static str, line: u32) -> ! {
    send(&Log(format_args!("panic at {}:{}: {}\n", file, line, args)));
    send(&RunAborted);
    loop {}
}

macro_rules! print {
    ($($arg:tt)*) => ($crate::send(&$crate::kernel_proto::Log(format_args!($($arg)*))));
}

macro_rules! println {
    ($fmt:expr) => (print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => (print!(concat!($fmt, "\n"), $($arg)*));
}

macro_rules! raise {
    ($name:expr, $message:expr, $param0:expr, $param1:expr, $param2:expr) => ({
        use cslice::AsCSlice;
        let exn = $crate::eh::Exception {
            name:     concat!("0:artiq.coredevice.exceptions.", $name).as_bytes().as_c_slice(),
            file:     file!().as_bytes().as_c_slice(),
            line:     line!(),
            column:   column!(),
            // https://github.com/rust-lang/rfcs/pull/1719
            function: "(Rust function)".as_bytes().as_c_slice(),
            message:  $message.as_bytes().as_c_slice(),
            param:    [$param0, $param1, $param2]
        };
        #[allow(unused_unsafe)]
        unsafe { $crate::eh::raise(&exn) }
    });
    ($name:expr, $message:expr) => ({
        raise!($name, $message, 0, 0, 0)
    });
}

pub mod eh;
mod api;
mod rtio;

static mut NOW: u64 = 0;
static mut LIBRARY: Option<Library<'static>> = None;

#[no_mangle]
pub extern fn send_to_core_log(text: CSlice<u8>) {
    match str::from_utf8(text.as_ref()) {
        Ok(s) => send(&LogSlice(s)),
        Err(e) => {
            send(&LogSlice(str::from_utf8(&text.as_ref()[..e.valid_up_to()]).unwrap()));
            send(&LogSlice("(invalid utf-8)\n"));
        }
    }
}

#[no_mangle]
pub extern fn send_to_rtio_log(timestamp: i64, text: CSlice<u8>) {
    rtio::log(timestamp, text.as_ref())
}

extern fn rpc_send(service: u32, tag: CSlice<u8>, data: *const *const ()) {
    while !rpc_queue::empty() {}
    send(&RpcSend {
        async:   false,
        service: service,
        tag:     tag.as_ref(),
        data:    data
    })
}

extern fn rpc_send_async(service: u32, tag: CSlice<u8>, data: *const *const ()) {
    while rpc_queue::full() {}
    rpc_queue::enqueue(|mut slice| {
        let length = {
            let mut writer = Cursor::new(&mut slice[4..]);
            rpc_proto::send_args(&mut writer, service, tag.as_ref(), data)?;
            writer.position()
        };
        proto::WriteExt::write_u32(&mut slice, length as u32)
    }).unwrap_or_else(|err| {
        assert!(err.kind() == std::io::ErrorKind::WriteZero);

        while !rpc_queue::empty() {}
        send(&RpcSend {
            async:   true,
            service: service,
            tag:     tag.as_ref(),
            data:    data
        })
    })
}

extern fn rpc_recv(slot: *mut ()) -> usize {
    send(&RpcRecvRequest(slot));
    recv!(&RpcRecvReply(ref result) => {
        match result {
            &Ok(alloc_size) => alloc_size,
            &Err(ref exception) =>
            unsafe {
                eh::raise(&eh::Exception {
                    name:     exception.name.as_bytes().as_c_slice(),
                    file:     exception.file.as_bytes().as_c_slice(),
                    line:     exception.line,
                    column:   exception.column,
                    function: exception.function.as_bytes().as_c_slice(),
                    message:  exception.message.as_bytes().as_c_slice(),
                    param:    exception.param
                })
            }
        }
    })
}

fn terminate(exception: &eh::Exception, mut backtrace: &mut [usize]) -> ! {
    let mut cursor = 0;
    for index in 0..backtrace.len() {
        if backtrace[index] > kernel_proto::KERNELCPU_PAYLOAD_ADDRESS {
            backtrace[cursor] = backtrace[index] - kernel_proto::KERNELCPU_PAYLOAD_ADDRESS;
            cursor += 1;
        }
    }
    let backtrace = &mut backtrace.as_mut()[0..cursor];

    send(&NowSave(unsafe { NOW }));
    send(&RunException {
        exception: kernel_proto::Exception {
            name:     str::from_utf8(exception.name.as_ref()).unwrap(),
            file:     str::from_utf8(exception.file.as_ref()).unwrap(),
            line:     exception.line,
            column:   exception.column,
            function: str::from_utf8(exception.function.as_ref()).unwrap(),
            message:  str::from_utf8(exception.message.as_ref()).unwrap(),
            param:    exception.param,
        },
        backtrace: backtrace
    });
    loop {}
}

extern fn watchdog_set(ms: i64) -> i32 {
    if ms < 0 {
        raise!("ValueError", "cannot set a watchdog with a negative timeout")
    }

    send(&WatchdogSetRequest { ms: ms as u64 });
    recv!(&WatchdogSetReply { id } => id) as i32
}

extern fn watchdog_clear(id: i32) {
    send(&WatchdogClear { id: id as usize })
}

extern fn cache_get(key: CSlice<u8>) -> CSlice<'static, i32> {
    send(&CacheGetRequest {
        key:   str::from_utf8(key.as_ref()).unwrap()
    });
    recv!(&CacheGetReply { value } => value.as_c_slice())
}

extern fn cache_put(key: CSlice<u8>, list: CSlice<i32>) {
    send(&CachePutRequest {
        key:   str::from_utf8(key.as_ref()).unwrap(),
        value: list.as_ref()
    });
    recv!(&CachePutReply { succeeded } => {
        if !succeeded {
            raise!("CacheError", "cannot put into a busy cache row")
        }
    })
}

extern fn i2c_start(busno: i32) {
    send(&I2cStartRequest { busno: busno as u8 });
}

extern fn i2c_stop(busno: i32) {
    send(&I2cStopRequest { busno: busno as u8 });
}

extern fn i2c_write(busno: i32, data: i32) -> bool {
    send(&I2cWriteRequest { busno: busno as u8, data: data as u8 });
    recv!(&I2cWriteReply { ack } => ack)
}

extern fn i2c_read(busno: i32, ack: bool) -> i32 {
    send(&I2cReadRequest { busno: busno as u8, ack: ack });
    recv!(&I2cReadReply { data } => data) as i32
}

static mut DMA_RECORDING: bool = false;

extern fn dma_record_start() {
    unsafe {
        if DMA_RECORDING {
            raise!("DMAError", "DMA is already recording")
        }

        let library = LIBRARY.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       dma_record_output as *const () as u32).unwrap();
        library.rebind(b"rtio_output_wide",
                       dma_record_output_wide as *const () as u32).unwrap();

        DMA_RECORDING = true;
        send(&DmaRecordStart);
    }
}

extern fn dma_record_stop(name: CSlice<u8>) {
    let name = str::from_utf8(name.as_ref()).unwrap();

    unsafe {
        if !DMA_RECORDING {
            raise!("DMAError", "DMA is not recording")
        }

        let library = LIBRARY.as_ref().unwrap();
        library.rebind(b"rtio_output",
                       rtio::output as *const () as u32).unwrap();
        library.rebind(b"rtio_output_wide",
                       rtio::output_wide as *const () as u32).unwrap();

        DMA_RECORDING = false;
        send(&DmaRecordStop(name));
    }
}

extern fn dma_record_output(timestamp: i64, channel: i32, address: i32, data: i32) {
    send(&DmaRecordAppend {
        timestamp: timestamp as u64,
        channel:   channel as u32,
        address:   address as u32,
        data:      &[data as u32]
    })
}

extern fn dma_record_output_wide(timestamp: i64, channel: i32, address: i32, data: CSlice<i32>) {
    assert!(data.len() <= 16); // enforce the hardware limit
    send(&DmaRecordAppend {
        timestamp: timestamp as u64,
        channel:   channel as u32,
        address:   address as u32,
        data:      unsafe { mem::transmute::<&[i32], &[u32]>(data.as_ref()) }
    })
}

extern fn dma_erase(name: CSlice<u8>) {
    let name = str::from_utf8(name.as_ref()).unwrap();

    send(&DmaEraseRequest(name));
}

unsafe fn rtio_arb_dma() {
    csr::rtio::arb_req_write(0);
    csr::rtio_dma::arb_req_write(1);
    while csr::rtio_dma::arb_gnt_read() == 0 {}
}

unsafe fn rtio_arb_regular() {
    csr::rtio_dma::arb_req_write(0);
    csr::rtio::arb_req_write(1);
    while csr::rtio::arb_gnt_read() == 0 {}
}

extern fn dma_playback(timestamp: i64, name: CSlice<u8>) {
    let name = str::from_utf8(name.as_ref()).unwrap();

    send(&DmaPlaybackRequest(name));
    let succeeded = recv!(&DmaPlaybackReply(data) => unsafe {
        // Here, we take advantage of the fact that DmaPlaybackReply always refers
        // to an entire heap allocation, which is 4-byte-aligned.
        let data = match data { Some(bytes) => bytes, None => return false };
        csr::rtio_dma::base_address_write(data.as_ptr() as u64);

        csr::rtio_dma::time_offset_write(timestamp as u64);
        rtio_arb_dma();
        csr::rtio_dma::enable_write(1);
        while csr::rtio_dma::enable_read() != 0 {}
        rtio_arb_regular();

        let status = csr::rtio_dma::error_status_read();
        let timestamp = csr::rtio_dma::error_timestamp_read();
        let channel = csr::rtio_dma::error_channel_read();
        if status & rtio::RTIO_O_STATUS_UNDERFLOW != 0 {
            csr::rtio_dma::error_underflow_reset_write(1);
            raise!("RTIOUnderflow",
                "RTIO underflow at {0} mu, channel {1}",
                timestamp as i64, channel as i64, 0)
        }
        if status & rtio::RTIO_O_STATUS_SEQUENCE_ERROR != 0 {
            csr::rtio_dma::error_sequence_error_reset_write(1);
            raise!("RTIOSequenceError",
                "RTIO sequence error at {0} mu, channel {1}",
                timestamp as i64, channel as i64, 0)
        }
        if status & rtio::RTIO_O_STATUS_COLLISION != 0 {
            csr::rtio_dma::error_collision_reset_write(1);
            raise!("RTIOCollision",
                "RTIO collision at {0} mu, channel {1}",
                timestamp as i64, channel as i64, 0)
        }
        if status & rtio::RTIO_O_STATUS_BUSY != 0 {
            csr::rtio_dma::error_busy_reset_write(1);
            raise!("RTIOBusy",
                "RTIO busy on channel {0}",
                channel as i64, 0, 0)
        }

        true
    });

    if !succeeded {
        println!("DMA trace called {:?} not found", name);
        raise!("DMAError",
            "DMA trace not found");
    }
}

unsafe fn attribute_writeback(typeinfo: *const ()) {
    struct Attr {
        offset: usize,
        tag:    CSlice<'static, u8>,
        name:   CSlice<'static, u8>
    }

    struct Type {
        attributes: *const *const Attr,
        objects:    *const *const ()
    }

    let mut tys = typeinfo as *const *const Type;
    while !(*tys).is_null() {
        let ty = *tys;
        tys = tys.offset(1);

        let mut objects = (*ty).objects;
        while !(*objects).is_null() {
            let object = *objects;
            objects = objects.offset(1);

            let mut attributes = (*ty).attributes;
            while !(*attributes).is_null() {
                let attribute = *attributes;
                attributes = attributes.offset(1);

                if (*attribute).tag.len() > 0 {
                    rpc_send_async(0, (*attribute).tag, [
                        &object as *const _ as *const (),
                        &(*attribute).name as *const _ as *const (),
                        (object as usize + (*attribute).offset) as *const ()
                    ].as_ptr());
                }
            }
        }
    }
}

#[no_mangle]
pub unsafe fn main() {
    let image = slice::from_raw_parts_mut(kernel_proto::KERNELCPU_PAYLOAD_ADDRESS as *mut u8,
                                          kernel_proto::KERNELCPU_LAST_ADDRESS -
                                          kernel_proto::KERNELCPU_PAYLOAD_ADDRESS);

    let library = recv!(&LoadRequest(library) => {
        match Library::load(library, image, &api::resolve) {
            Err(error) => {
                send(&LoadReply(Err(error)));
                loop {}
            },
            Ok(library) => {
                send(&LoadReply(Ok(())));
                library
            }
        }
    });

    let __bss_start = library.lookup(b"__bss_start").unwrap();
    let _end = library.lookup(b"_end").unwrap();
    let __modinit__ = library.lookup(b"__modinit__").unwrap();
    let typeinfo = library.lookup(b"typeinfo");

    LIBRARY = Some(library);

    ptr::write_bytes(__bss_start as *mut u8, 0, (_end - __bss_start) as usize);

    send(&NowInitRequest);
    recv!(&NowInitReply(now) => NOW = now);
    (mem::transmute::<u32, fn()>(__modinit__))();
    send(&NowSave(NOW));

    if let Some(typeinfo) = typeinfo {
        attribute_writeback(typeinfo as *const ());
    }

    send(&RunFinished);

    loop {}
}

#[no_mangle]
pub extern fn exception_handler(vect: u32, _regs: *const u32, pc: u32, ea: u32) {
    panic!("exception {:?} at PC 0x{:x}, EA 0x{:x}", vect, pc, ea)
}

// We don't export this because libbase does.
// #[no_mangle]
pub extern fn abort() {
    panic!("aborted")
}
