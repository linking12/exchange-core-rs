use std::cell::RefCell;
use std::rc::Rc;

use crate::core::common::cmd::order_command::OrderCommand;

pub trait CommandSubmitter {
    fn submit(&mut self, cmd: OrderCommand);
    fn take_pending(&mut self) -> Vec<OrderCommand> {
        Vec::new()
    }
}

#[derive(Default, Clone)]
pub struct CommandSubmitterHandle(Option<Rc<RefCell<dyn CommandSubmitter>>>);

impl CommandSubmitterHandle {
    pub fn set(&mut self, submitter: Rc<RefCell<dyn CommandSubmitter>>) {
        self.0 = Some(submitter);
    }

    pub fn submit(&mut self, cmd: OrderCommand) {
        if let Some(s) = &self.0 {
            s.borrow_mut().submit(cmd);
        }
    }
}

impl std::fmt::Debug for CommandSubmitterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CommandSubmitterHandle").field(&self.0.as_ref().map(|_| "<submitter>")).finish()
    }
}

#[derive(Default)]
pub struct LocalCommandSubmitter {
    queue: Vec<OrderCommand>,
}
impl CommandSubmitter for LocalCommandSubmitter {
    fn submit(&mut self, cmd: OrderCommand) {
        self.queue.push(cmd);
    }
    fn take_pending(&mut self) -> Vec<OrderCommand> {
        std::mem::take(&mut self.queue)
    }
}

#[cfg(test)]
pub struct TestCommandCollector(pub Rc<RefCell<Vec<OrderCommand>>>);
#[cfg(test)]
impl CommandSubmitter for TestCommandCollector {
    fn submit(&mut self, cmd: OrderCommand) {
        self.0.borrow_mut().push(cmd);
    }
    fn take_pending(&mut self) -> Vec<OrderCommand> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}
