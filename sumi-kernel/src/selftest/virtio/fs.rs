use crate::kprint;
use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::SelfTest;
use sumi_abi::fuse::FUSE_ROOT_ID;

pub(crate) const TESTS: [SelfTest; 2] = [
    SelfTest {
        name: "create_write_read",
        func: test_create_write_read,
    },
    SelfTest {
        name: "read_print",
        func: test_read_print,
    },
];

fn test_create_write_read() -> bool {
    let fs = crate::fs();

    let flags: u32 = 2 | 0o100 | 0o1000; // O_RDWR | O_CREAT | O_TRUNC
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_rw.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let data = [b'P', b'A', b'S', b'S'];
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let written = match fs.write(open.fh, 0, data_phys, 4) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    let buf = [0u8; 4];
    let buf_phys = VirtioFsClient::v2p(buf.as_ptr());
    let read = match fs.read(open.fh, 0, buf_phys, 4) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    written == 4 && read == 4 && &buf == b"PASS"
}

fn test_read_print() -> bool {
    let fs = crate::fs();

    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_print.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let msg = *b"hello from kernel selftest!\n";
    let msg_phys = VirtioFsClient::v2p(msg.as_ptr());
    if fs.write(open.fh, 0, msg_phys, msg.len() as u32).is_err() {
        fs.release(open.fh);
        fs.forget(entry.nodeid, 1);
        return false;
    }

    let buf = [0u8; 64];
    let buf_phys = VirtioFsClient::v2p(buf.as_ptr());
    let n = match fs.read(open.fh, 0, buf_phys, buf.len() as u32) {
        Ok(n) => n as usize,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    kprint!("    | ");
    for &b in &buf[..n] {
        crate::arch::debugcon_write_byte(b);
    }

    n == msg.len()
}
