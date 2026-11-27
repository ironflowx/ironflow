//! D23's barrier in the real binary. The parent gives the fixture one end of a socket pair.

use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, Thread};

use ironflowx_core::{EnvRef, FaultPoint, KeyId, Secret, set_fault_hook, set_key_hook};

static STOP: OnceLock<(String, u64)> = OnceLock::new();
static CROSSINGS: AtomicU64 = AtomicU64::new(0);
static SOCKET: OnceLock<UnixStream> = OnceLock::new();
static PARKED: OnceLock<Thread> = OnceLock::new();
static RELEASED: AtomicBool = AtomicBool::new(false);

pub fn install() {
    let Ok(config) = std::env::var("IFX_SERVER_CRASH") else {
        return;
    };
    let (point, crossing) = config.split_once(' ').expect("crash point and crossing");
    let crossing = crossing.parse().expect("numeric crash crossing");
    let socket = UnixStream::from(
        io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .expect("fixture stdin"),
    );
    let peer = socket
        .peer_addr()
        .expect("crash fixture stdin is a connected Unix socket");
    assert!(peer.is_unnamed(), "crash fixture peer must be unnamed");
    let mut watch = socket.try_clone().expect("duplicate the fixture socket");
    STOP.set((point.to_owned(), crossing))
        .expect("install only once");
    SOCKET.set(socket).expect("install only once");
    set_key_hook(tell);
    set_fault_hook(at_point);
    thread::spawn(move || {
        // The parent may release a parked barrier. EOF exits an orphaned child.
        let mut command = [0];
        if watch.read_exact(&mut command).is_ok() && command == [1] {
            let parked = PARKED.get().expect("barrier reached before release");
            RELEASED.store(true, Ordering::Release);
            parked.unpark();
            let _ = watch.read_exact(&mut command);
        }
        std::process::exit(1);
    });
}

fn tell(env: EnvRef, id: KeyId, secret: Option<&Secret>) {
    let mut socket = SOCKET.get().expect("installed socket");
    writeln!(socket, "{env} {id} {}", secret.map_or("-", Secret::expose))
        .expect("write the private key notification");
}

fn at_point(point: FaultPoint) {
    let (name, crossing) = STOP.get().expect("installed point");
    if point == FaultPoint::ReadWaitingForFold {
        if name == FaultPoint::AfterAck.name() && PARKED.get().is_some() {
            announce(point);
        }
        return;
    }
    if point.name() == name && CROSSINGS.fetch_add(1, Ordering::Relaxed) + 1 == *crossing {
        PARKED
            .set(thread::current())
            .expect("only one parked crossing");
        announce(point);
        while !RELEASED.load(Ordering::Acquire) {
            thread::park();
        }
    }
}

fn announce(point: FaultPoint) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "at {}", point.name()).expect("write the barrier");
    stdout.flush().expect("flush the barrier");
}
