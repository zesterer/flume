use super::*;

use core::{
    cell::RefCell,
    marker::PhantomData,
};
use alloc::{
    rc::Rc,
    collections::BTreeMap,
};
use futures_util::future::{FutureExt, LocalBoxFuture, select_all};

pub type Token = usize;

#[derive(Default)]
pub struct Selector<'a> {
    selections: Vec<LocalBoxFuture<'a, Token>>,
    token_map: BTreeMap<Token, usize>,
}

impl<'a> Selector<'a> {
    pub fn new() -> Self { Self::default() }

    fn make_selection<F: Future + 'a>(&mut self, token: Token, fut: F) -> Selection<'a, F::Output> {
        let item_cell = Rc::new(RefCell::new(None));
        self.token_map.insert(token, self.selections.len());
        self.selections.push(fut
            .map({
                let item_cell = item_cell.clone();
                move |res| { *item_cell.borrow_mut() = Some(res); token }
            })
            .boxed_local());

        Selection {
            item_cell,
            phantom: PhantomData,
        }
    }

    pub fn send<T>(&mut self, token: Token, sender: &'a Sender<T>, item: T) -> Selection<'a, Result<(), T>> {
        self.make_selection(token, sender.send_async(item))
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn send_timeout<T>(&mut self, token: Token, sender: &'a Sender<T>, item: T, timeout: Duration) -> Selection<'a, Result<(), T>> {
        self.make_selection(token, sender.send_timeout_async(item, timeout))
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn send_deadline<T>(&mut self, token: Token, sender: &'a Sender<T>, item: T, deadline: Instant) -> Selection<'a, Result<(), T>> {
        self.make_selection(token, sender.send_deadline_async(item, deadline))
    }

    pub fn recv<T>(&mut self, token: Token, receiver: &'a Receiver<T>) -> Selection<'a, Result<T, ()>> {
        self.make_selection(token, receiver.recv_async())
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn recv_timeout<T>(&mut self, token: Token, receiver: &'a Receiver<T>, timeout: Duration) -> Selection<'a, Result<T, ()>> {
        self.make_selection(token, receiver.recv_timeout_async(timeout))
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn recv_deadline<T>(&mut self, token: Token, receiver: &'a Receiver<T>, deadline: Instant) -> Selection<'a, Result<T, ()>> {
        self.make_selection(token, receiver.recv_deadline_async(deadline))
    }

    pub async fn wait_async(self) -> Token {
        select_all(self.selections).await.1
    }

    pub fn wait(self) -> Token {
        pollster::block_on(self.wait_async())
    }
}

pub struct Selection<'a, T> {
    item_cell: Rc<RefCell<Option<T>>>,
    phantom: PhantomData<&'a ()>,

}

impl<'a, T> Selection<'a, T> {
    pub fn try_get(&self) -> Option<T> {
        self.item_cell.borrow_mut().take()
    }

    pub fn get(self) -> T {
        self.try_get().unwrap()
    }
}
