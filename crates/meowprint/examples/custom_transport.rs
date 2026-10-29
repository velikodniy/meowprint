//! Record printer writes in memory with a custom transport and generated row source.
use async_trait::async_trait;
use meowprint::{
    Error, Gt01, Gt01Options, PixelFormat, Printer, Result, RowSource, Transport, TransportEvent,
    WriteChannel,
};

#[derive(Default)]
struct MemoryTransport {
    writes: Vec<(WriteChannel, Vec<u8>)>,
}
#[async_trait]
impl Transport for MemoryTransport {
    async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
        self.writes.push((channel, bytes.to_vec()));
        Ok(())
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        std::future::pending().await
    }
    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

struct GeneratedRows {
    width: u32,
    next: u32,
}
impl RowSource for GeneratedRows {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        8
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Mono
    }
    fn read_row(&mut self, output: &mut [u8]) -> impl Future<Output = Result<()>> + Send {
        if self.next >= self.height() {
            return std::future::ready(Err(Error::InvalidInput(
                "No generated rows remain.".into(),
            )));
        }
        output.fill(0xff);
        self.next += 1;
        std::future::ready(Ok(()))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let driver = Gt01;
    let mut printer = Printer::new(MemoryTransport::default(), driver);
    let result = async {
        let mut rows = GeneratedRows {
            width: printer.printable_width(),
            next: 0,
        };
        printer.print(&mut rows, &Gt01Options::default()).await
    }
    .await;
    let disconnected = printer.disconnect().await;
    let report = result?;
    println!(
        "image_rows={} bytes_submitted={} completion={:?}",
        report.image_rows, report.bytes_submitted, report.completion
    );
    disconnected
}
