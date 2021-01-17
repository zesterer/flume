use super::*;

pub struct SendSink<'a, T> {
    pub(crate) send: Option<SendFut<'a, T>>,
}

impl<'a, T> futures_sink::Sink<T> for SendSink<'a, T> {
    type Error = T;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let send = self.send.take().unwrap();
        let terminated = send.is_terminated();
        self.send = Some(send);

        if terminated {
            Poll::Ready(Ok(()))
        } else {
            Pin::new(self.send.as_mut().unwrap()).poll(cx)
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        debug_assert!(
            self.send.as_ref().unwrap().is_terminated(),
            "`start_send` called before polling has terminated",
        );

        self.send = Some(Channel::send_async(self.send.take().unwrap().into_sender(), item));

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        // `poll_ready` already flushes the current item, so just use that
        self.poll_ready(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        // `poll_flush` already flushes the current item, so just use that
        self.poll_flush(cx)
    }
}
