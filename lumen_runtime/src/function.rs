use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::code::Code;
use crate::process::stack::frame::Frame;
use crate::process::ModuleFunctionArity;
use crate::term::{Tag, Term};

pub struct Function {
    #[allow(dead_code)]
    header: Term,
    module_function_arity: Arc<ModuleFunctionArity>,
    code: Code,
}

impl Function {
    pub fn new(module_function_arity: Arc<ModuleFunctionArity>, code: Code) -> Self {
        Self {
            header: Term {
                tagged: Tag::Function as usize,
            },
            module_function_arity,
            code,
        }
    }

    pub fn frame_with_arguments(&self, argument_vec: Vec<Term>) -> Option<Frame> {
        if argument_vec.len() == self.module_function_arity.arity {
            let mut frame = self.frame();

            for argument in argument_vec.into_iter().rev() {
                frame.push(argument)
            }

            Some(frame)
        } else {
            None
        }
    }

    pub fn module_function_arity(&self) -> Arc<ModuleFunctionArity> {
        Arc::clone(&self.module_function_arity)
    }

    // Private

    fn frame(&self) -> Frame {
        Frame::new(Arc::clone(&self.module_function_arity), self.code)
    }
}

impl Hash for Function {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.code as usize);
    }
}

impl PartialEq for Function {
    fn eq(&self, other: &Function) -> bool {
        (self.code as usize) == (other.code as usize)
    }
}
