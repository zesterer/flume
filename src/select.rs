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

    pub fn send<T>(&mut self, token: Token, sender: &'a Sender<T>, item: T) -> Selection<'a, Result<(), T>> {
        let item_cell = Rc::new(RefCell::new(None));
        self.token_map.insert(token, self.selections.len());
        self.selections.push(sender
            .send_async(item)
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

    pub fn recv<T>(&mut self, token: Token, receiver: &'a Receiver<T>) -> Selection<'a, Result<T, ()>> {
        let item_cell = Rc::new(RefCell::new(None));
        self.token_map.insert(token, self.selections.len());
        self.selections.push(receiver
            .recv_async()
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
