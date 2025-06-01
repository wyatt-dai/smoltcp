mod utils;

use std::cmp;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::sync::{Arc,Mutex};
use embassy_executor::{Spawner,task};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{wait as phy_wait, Device, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

//  #[embassy_executor::task]
//  async fn recv_task(device:TunTapInterface) {
//     loop {
//          device.poll_fd();
//          Timer::after(Duration::from_millis(10)).await;
//     }
// }

const AMOUNT: usize = 1_000_000_000;


enum Client {
    Reader,
    Writer,
}
fn client(kind: Client) {
    let port = match kind {
        Client::Reader => 1234,
        Client::Writer => 1235,
    };
    let mut stream = TcpStream::connect(("192.168.69.1", port)).unwrap();
    let mut buffer = vec![0; 1_000_000];

    let start = Instant::now();

    let mut processed = 0;
    while processed < AMOUNT {
        let length = cmp::min(buffer.len(), AMOUNT - processed);
        let result = match kind {
            Client::Reader => stream.read(&mut buffer[..length]),
            Client::Writer => stream.write(&buffer[..length]),
        };
        match result {
            Ok(0) => break,
            Ok(result) => {
                // print!("(P:{})", result);
                processed += result
            }
            Err(err) => panic!("cannot process: {err}"),
        }
    }

    let end = Instant::now();

    let elapsed = (end - start).total_millis() as f64 / 1000.0;

    println!("throughput: {:.3} Gbps", AMOUNT as f64 / elapsed / 0.125e9);

    CLIENT_DONE.store(true, Ordering::SeqCst);
}
static CLIENT_DONE: AtomicBool = AtomicBool::new(false);



#[embassy_executor::main]
 async fn main(spawner: Spawner){
    #[cfg(feature = "log")]
    utils::setup_logging("info");

    let (mut opts, mut free) = utils::create_options();
    utils::add_tuntap_options(&mut opts, &mut free);
    utils::add_middleware_options(&mut opts, &mut free);
    free.push("MODE");

    let mut matches = utils::parse_options(&opts, free);
    let mut device = utils::parse_tuntap_options(&mut matches);
    let fd = device.as_raw_fd();
    let mut device =
    utils::parse_middleware_options(&mut matches, device, /*loopback=*/ false);
    let mode = match matches.free[0].as_ref() {
        "reader" => Client::Reader,
        "writer" => Client::Writer,
        _ => panic!("invalid mode"),
    };

    let tcp1_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp1_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp1_socket = tcp::Socket::new(tcp1_rx_buffer, tcp1_tx_buffer);

    let tcp2_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp2_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp2_socket = tcp::Socket::new(tcp2_rx_buffer, tcp2_tx_buffer);

    let mut config = match device.capabilities().medium {
        Medium::Ethernet => {
            Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into())
        }
        Medium::Ip => Config::new(smoltcp::wire::HardwareAddress::Ip),
        Medium::Ieee802154 => todo!(),
    };
    config.random_seed = rand::random();

    let mut iface = Interface::new(config, &mut device, Instant::now());
    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs
            .push(IpCidr::new(IpAddress::v4(192, 168, 69, 1), 24))
            .unwrap();
    });
    let mut sockets = SocketSet::new(vec![]);
    let tcp1_handle = sockets.add(tcp1_socket);
    let tcp2_handle = sockets.add(tcp2_socket);
    let default_timeout = Some(Duration::from_millis(1000));
    thread::spawn(move || client(mode));
    let iface = Arc::new(Mutex::new(iface));
    let sockets = Arc::new(Mutex::new(sockets));
    let fd = fd; 
    let iface_clone = Arc::clone(&iface);
    let sockets_clone = Arc::clone(&sockets);
    let processed = Arc::new(Mutex::new(0usize));
    let processed_clone = Arc::clone(&processed);
    let process_phase = Arc::new(AtomicBool::new(false));
    let process_phase_clone = Arc::clone(&process_phase);
    thread::spawn(move || {
        while !CLIENT_DONE.load(Ordering::SeqCst) {
            if process_phase_clone.load(Ordering::SeqCst) {
            let timestamp = Instant::now();
            let mut iface_locked = iface_clone.lock().unwrap(); 
            let mut sockets_locked = sockets_clone.lock().unwrap();
            let mut process_locked=processed_clone.lock().unwrap();

            let socket = sockets_locked.get_mut::<tcp::Socket>(tcp1_handle);
            if !socket.is_open() {
                socket.listen(1234).unwrap();
            }
            while socket.can_send() && *process_locked < AMOUNT {
                let len = socket.send(|buf| {
                    let len = cmp::min(buf.len(), AMOUNT - *process_locked);
                    (len, len)
                }, &mut iface_locked).unwrap();
                *process_locked += len;
            }
            // let socket = sockets_locked.get_mut::<tcp::Socket>(tcp2_handle);
            // if !socket.is_open() {
            //     socket.listen(1235).unwrap();
            // }
            // while socket.can_recv() && *process_locked < AMOUNT {
            //     let len = socket.recv(|buf| {
            //         let len = cmp::min(buf.len(), AMOUNT - *process_locked);
            //         (len, len)
            //     }).unwrap();
            //     *process_locked += len;
            // }
            drop(iface_locked);
            drop(sockets_locked);
            process_phase_clone.store(false, Ordering::SeqCst);
        }
        }
    });
    while !CLIENT_DONE.load(Ordering::SeqCst){
        if !process_phase.load(Ordering::SeqCst) {
        let timestamp = Instant::now();
        let mut iface_locked = iface.lock().unwrap();
        let mut sockets_locked = sockets.lock().unwrap();
        iface_locked.poll(timestamp,&mut device, &mut sockets_locked);
        match iface_locked.poll_at(timestamp, &*sockets_locked) {
            Some(poll_at) if timestamp < poll_at => {
                drop(iface_locked);
                drop(sockets_locked);
                phy_wait(fd, Some(poll_at - timestamp)).expect("wait error");
            }
            Some(_) => {
                drop(iface_locked);
                drop(sockets_locked);
                phy_wait(fd, default_timeout).expect("wait error");
            }
            None => {
                drop(iface_locked);
                drop(sockets_locked);
                iface.lock().unwrap().wait_for_next_action().await;
            }
        }
        process_phase.store(true, Ordering::SeqCst);
    }
    }
}
    
//     while !CLIENT_DONE.load(Ordering::SeqCst) {
//         let timestamp = Instant::now();
//         // tcp:1234: emit data
//         let socket = sockets.get_mut::<tcp::Socket>(tcp1_handle);
//         if !socket.is_open() {
//             socket.listen(1234).unwrap();
//         }

//         while socket.can_send() && processed < AMOUNT {
//             let length = socket
//                 .send(|buffer| {
//                     let length = cmp::min(buffer.len(), AMOUNT - processed);
//                     (length, length)
//                 },iface)
//                 .unwrap();
//             processed += length;
//         }

//         // tcp:1235: sink data
//         let socket = sockets.get_mut::<tcp::Socket>(tcp2_handle);
//         if !socket.is_open() {
//             socket.listen(1235).unwrap();
//         }

//         while socket.can_recv() && processed < AMOUNT {
//             let length = socket
//                 .recv(|buffer| {
//                     let length = cmp::min(buffer.len(), AMOUNT - processed);
//                     (length, length)
//                 })
//                 .unwrap();
//             processed += length;
//         }
//         match iface.poll_at(timestamp, &sockets) {
//             Some(poll_at) if timestamp < poll_at => {
//                 phy_wait(fd, Some(poll_at - timestamp)).expect("wait error");
//             }
//             Some(_) =>{ phy_wait(fd, default_timeout).expect("wait error");}
//             None => {
//                 iface.wait_for_next_action().await;
//             }
//         }
//     }
// }


// pub async fn task_poll_static<D>(
//     iface: Arc<Mutex< Interface<'static, 'static, 'static>>>,
//     sockets: Arc<Mutex<SocketSet<'static>>>,
//     device: &'static mut D,
//     fd: i32,
//     default_timeout: Option<Duration>,
// ) where
//     D: Device + Unpin + 'static,
// {
//     loop {
//         if CLIENT_DONE.load(Ordering::SeqCst) {
//             break;
//         }

//         let timestamp = Instant::now();

//         {
//             let mut iface = iface.lock().await;
//             let mut sockets = sockets.lock().await;
//             iface.poll(timestamp, device, &mut sockets);
//         }

//         let poll_at = {
//             let iface = iface.lock().await;
//             let sockets = sockets.lock().await;
//             iface.poll_at(timestamp, &*sockets)
//         };

//         match poll_at {
//             Some(when) if timestamp < when => {
//                 phy_wait(fd, Some(when - timestamp)).expect("wait error");
//             }
//             Some(_) => {
//                 phy_wait(fd, default_timeout).expect("wait error");
//             }
//             None => {
//                 poll_fn(|cx| {
//                     iface.poll_waker.register(cx.waker());
//                     Poll::Pending
//                 }).await;
//             }
//         }
//     }
// }

// /// SEND_RECV 任务：处理 TCP 读写逻辑
// pub async fn task_send_recv_static(
//     iface: Arc<Mutex<Interface<'static, 'static, 'static>>>,
//     sockets: Arc<Mutex<SocketSet<'static>>>,
//     tcp1_handle: SocketHandle,
//     tcp2_handle: SocketHandle,
//     processed:Arc<Mutex<i32>>,
// ) {
//     loop {
//         if CLIENT_DONE.load(Ordering::SeqCst) {
//             break;
//         }

//         let mut iface_locked = iface.lock().await;
//         let mut sockets_locked = sockets.lock().await;
//         let mut processed_locked = processed.lock().await;

//         {
//             let socket1 = sockets_locked.get_mut::<TcpSocket>(tcp1_handle);
//             if !socket1.is_open() {
//                 socket1.listen(1234).unwrap();
//             }

//             while socket1.can_send() && *processed_locked < AMOUNT {
//                 let len = socket1.send(|buf| {
//                     let len = cmp::min(buf.len(), AMOUNT - *processed_locked);
//                     (len, len)
//                 }, &mut *iface_locked).unwrap();

//                 *processed_locked += len;
//             }
//         }

//         {
//             let socket2 = sockets_locked.get_mut::<TcpSocket>(tcp2_handle);
//             if !socket2.is_open() {
//                 socket2.listen(1235).unwrap();
//             }

//             while socket2.can_recv() && *processed_locked < AMOUNT {
//                 let len = socket2.recv(|buf| {
//                     let len = cmp::min(buf.len(), AMOUNT - *processed_locked);
//                     (len, len)
//                 }).unwrap();

//                 *processed_locked += len;
//             }
//         }

//         drop(processed_locked);
//         drop(sockets_locked);
//         drop(iface_locked);
//     }
// }