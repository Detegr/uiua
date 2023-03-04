use std::{cmp::Ordering, fmt, mem::swap};

use crate::{
    array::Array,
    ast::{BinOp, FunctionId},
    builtin::{Op1, Op2},
    compile::Assembly,
    lex::Span,
    list::List,
    value::{type_line, Function, Partial, Type, Value},
    RuntimeResult, TraceFrame, UiuaError, UiuaResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Instr {
    Comment(String),
    Push(Value),
    Constant(usize),
    List(usize),
    Array(usize),
    /// Copy the nth value from the top of the stack
    CopyRel(usize),
    /// Copy the nth value from the bottom of the stack
    CopyAbs(usize),
    Call {
        args: usize,
        span: usize,
    },
    Return,
    Jump(isize),
    PopJumpIf(isize, bool),
    JumpIfElsePop(isize, bool),
    BinOp(BinOp, Box<Span>),
    BuiltinOp1(Op1),
    BuiltinOp2(Op2),
    DestructureList(usize, Box<Span>),
    PushUnresolvedFunction(Box<FunctionId>),
    Dud,
}

fn _keep_instr_small(_: std::convert::Infallible) {
    let _: [u8; 32] = unsafe { std::mem::transmute(Instr::Dud) };
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instr::Comment(s) => write!(f, "\n// {}", s),
            instr => write!(f, "{instr:?}"),
        }
    }
}

struct StackFrame {
    stack_size: usize,
    function: Function,
    ret: usize,
    call_span: usize,
}

const DBG: bool = false;
macro_rules! dprintln {
    ($($arg:tt)*) => {
        if DBG {
            println!($($arg)*);
        }
    };
}

#[derive(Default)]
pub struct Vm {
    call_stack: Vec<StackFrame>,
    pub stack: Vec<Value>,
    pc: Option<usize>,
}

impl Vm {
    pub fn run_assembly(&mut self, assembly: &Assembly) -> UiuaResult<Option<Value>> {
        match self.run_assembly_inner(assembly) {
            Ok(val) => Ok(val),
            Err(error) => {
                let mut trace = Vec::new();
                for frame in self.call_stack.iter().rev() {
                    let info = assembly.function_info(frame.function);
                    trace.push(TraceFrame {
                        id: info.id.clone(),
                        span: assembly.call_spans[frame.call_span].clone(),
                    });
                }
                Err(UiuaError::Run {
                    error: error.into(),
                    trace,
                })
            }
        }
    }
    fn run_assembly_inner(&mut self, assembly: &Assembly) -> RuntimeResult<Option<Value>> {
        let stack = &mut self.stack;
        let call_stack = &mut self.call_stack;
        dprintln!("Running...");
        let pc = if let Some(pc) = &mut self.pc {
            *pc -= 1;
            pc
        } else {
            self.pc = Some(assembly.start);
            self.pc.as_mut().unwrap()
        };
        #[cfg(feature = "profile")]
        let mut i = 0;
        while *pc < assembly.instrs.len() {
            #[cfg(feature = "profile")]
            if i % 100_000 == 0 {
                puffin::GlobalProfiler::lock().new_frame();
            }
            #[cfg(feature = "profile")]
            {
                i += 1;
            }
            #[cfg(feature = "profile")]
            puffin::profile_scope!("instr loop");
            let instr = &assembly.instrs[*pc];
            dprintln!("{pc:>3} {instr}");
            match instr {
                Instr::Comment(_) => {}
                Instr::Push(v) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("push");
                    stack.push(v.clone())
                }
                Instr::Constant(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("constant");
                    stack.push(assembly.constants[*n].clone())
                }
                Instr::List(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("list");
                    let list: List = stack.drain(stack.len() - *n..).collect();
                    stack.push(list.into());
                }
                Instr::Array(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("list");
                    let array: Array = stack.drain(stack.len() - *n..).collect();
                    stack.push(array.into());
                }
                Instr::CopyRel(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("copy");
                    stack.push(stack[stack.len() - *n].clone())
                }
                Instr::CopyAbs(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("copy");
                    stack.push(stack[*n].clone())
                }
                Instr::Call {
                    args: call_arg_count,
                    span,
                } => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("call");
                    // Table and function for getting call information from values
                    type CallFn =
                        fn(Value, usize, &Assembly) -> RuntimeResult<(Function, Vec<Value>)>;
                    const fn call_fn(ty: Type) -> CallFn {
                        unsafe {
                            match ty {
                                Type::Function => |val, _, _| Ok((val.data.function, Vec::new())),
                                Type::Partial => |val, _, _| {
                                    let partial = &val.data.partial;
                                    Ok((partial.function, partial.args.clone()))
                                },
                                _ => |val, span, assembly| {
                                    let message = format!("cannot call {}", val.ty());
                                    Err(assembly.call_spans[span].error(message))
                                },
                            }
                        }
                    }
                    static CALL_TABLE: [CallFn; Type::ARITY] = type_line!(call_fn);
                    // Get the call information
                    let value = stack.pop().unwrap();
                    let (function, args) = CALL_TABLE[value.ty() as usize](value, *span, assembly)?;
                    // Handle partial arguments
                    let partial_count = args.len();
                    let arg_count = call_arg_count + partial_count;
                    for arg in args {
                        stack.push(arg);
                    }
                    if partial_count > 0 {
                        dprintln!("{stack:?}");
                    }
                    // Call
                    let info = assembly.function_info(function);
                    match arg_count.cmp(&info.params) {
                        Ordering::Less => {
                            let partial = Partial {
                                function,
                                args: stack.drain(stack.len() - arg_count..).collect(),
                            };
                            stack.push(partial.into());
                        }
                        Ordering::Equal => {
                            call_stack.push(StackFrame {
                                stack_size: stack.len() - arg_count,
                                function,
                                ret: *pc + 1,
                                call_span: *span,
                            });
                            *pc = function.0;
                            continue;
                        }
                        Ordering::Greater => {
                            let message = format!(
                                "too many arguments: expected {}, got {}",
                                info.params, arg_count
                            );
                            return Err(assembly.call_spans[*span].clone().sp(message));
                        }
                    }
                }
                Instr::Return => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("return");
                    if let Some(frame) = call_stack.pop() {
                        let value = stack.pop().unwrap();
                        *pc = frame.ret;
                        stack.truncate(frame.stack_size);
                        stack.push(value);
                        dprintln!("{:?}", stack);
                        continue;
                    } else {
                        break;
                    }
                }
                Instr::Jump(delta) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump");
                    *pc = pc.wrapping_add_signed(*delta);
                    continue;
                }
                Instr::PopJumpIf(delta, cond) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump_if");
                    let val = stack.pop().unwrap();
                    if val.is_truthy() == *cond {
                        *pc = pc.wrapping_add_signed(*delta);
                        continue;
                    }
                }
                Instr::JumpIfElsePop(delta, cond) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump_if_else_pop");
                    let val = stack.last().unwrap();
                    if val.is_truthy() == *cond {
                        *pc = pc.wrapping_add_signed(*delta);
                        continue;
                    } else {
                        stack.pop();
                    }
                }
                Instr::BinOp(op, span) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("bin_op");
                    let (left, right) = {
                        #[cfg(feature = "profile")]
                        puffin::profile_scope!("bin_args");
                        let right = stack.pop().unwrap();
                        let left = stack.last_mut().unwrap();
                        (left, right)
                    };
                    left.bin_op(right, *op, span)?;
                }
                Instr::BuiltinOp1(op) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("builtin_op1");
                    let val = stack.last_mut().unwrap();
                    val.op1(*op, &Span::Builtin)?;
                }
                Instr::BuiltinOp2(op) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("builtin_op2");
                    let (left, mut right) = {
                        #[cfg(feature = "profile")]
                        puffin::profile_scope!("builtin_op2_args");
                        let right = stack.pop().unwrap();
                        let left = stack.last_mut().unwrap();
                        (left, right)
                    };
                    swap(left, &mut right);
                    left.op2(right, *op, &Span::Builtin)?;
                }
                Instr::DestructureList(_n, _span) => {
                    // let list = match stack.pop().unwrap() {
                    //     Value::List(list) if *n == list.len() => list,
                    //     Value::List(list) => {
                    //         let message =
                    //             format!("cannot destructure list of {} as list of {n}", list.len());
                    //         return Err(span.sp(message).into());
                    //     }
                    //     val => {
                    //         let message =
                    //             format!("cannot destructure {} as list of {}", val.ty(), n);
                    //         return Err(span.sp(message).into());
                    //     }
                    // };
                    // for val in list.into_iter().rev() {
                    //     stack.push(val);
                    // }
                }
                Instr::PushUnresolvedFunction(id) => {
                    panic!("unresolved function: {id:?}")
                }
                Instr::Dud => {
                    panic!("unresolved instruction")
                }
            }
            dprintln!("{:?}", stack);
            *pc += 1;
        }
        Ok(stack.pop())
    }
}