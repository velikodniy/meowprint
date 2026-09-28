//! Record printer writes in memory with a custom transport.
use async_trait::async_trait;
use meowprint::{
    Gt01, Gt01Options, PixelFormat, Printer, Result, Transport, TransportEvent, WriteChannel,
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
#[tokio::main]
async fn main() -> Result<()> {
    let driver = Gt01;
    let mut printer = Printer::new(MemoryTransport::default(), driver);
    let result = async {
        let job = printer.prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?;
        // Enable the optional image feature to inspect the prepared pixels.
        #[cfg(feature = "image")]
        assert_eq!(job.preview().dimensions(), (384, 1));
        job.print().await
    }
    .await;
    let disconnected = printer.disconnect().await;
    assert_eq!(result?.image_rows, 1);
    disconnected
}
