//! Preview a job without Bluetooth access or a Tokio runtime.
use meowprint::{Gt01, Gt01Options, NoOpTransport, PixelFormat, Printer, Result};

fn main() -> Result<()> {
    let mut printer = Printer::new(NoOpTransport, Gt01);
    let job = printer.prepare(vec![0x81; 48], PixelFormat::Mono, &Gt01Options::default())?;
    let preview = job.preview();
    assert_eq!(preview.dimensions(), (384, 1));
    assert_eq!(preview.get_pixel(0, 0)[0], 0);
    assert_eq!(preview.get_pixel(1, 0)[0], 255);
    // Discarding the job releases its printer without attempting communication.
    drop(job);
    assert!(printer.is_usable());
    Ok(())
}
