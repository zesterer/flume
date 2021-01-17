use super::*;

pub struct RecvStream<'a, T> {
    pub(crate) recv: Option<RecvFut<'a, T>>,
}

impl<'a, T> futures_core::Stream for RecvStream<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let recv = self.recv.take().unwrap();
        if recv.is_terminated() {
            self.recv = Some(Channel::recv_async(recv.into_receiver()));
        }

        match Pin::new(self.recv.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(x) => Poll::Ready(x.ok()),
        }
    }
}
