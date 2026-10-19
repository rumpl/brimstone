//! Host integration primitives. Persistent roots are scoped to a single context.
use std::{
    cell::UnsafeCell,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::runtime::{
    Context, EvalResult, Handle, Realm, Value,
    gc::HeapVisitor,
    module::{module::DynModule, source_text_module::ModuleRequest},
    string_value::FlatString,
};

struct Root {
    value: UnsafeCell<Value>,
    context: usize,
}

/// A GC-updated value owned by the host. It must not be used after its context is dropped.
#[derive(Clone)]
pub struct Persistent(Rc<Root>);

impl Persistent {
    pub fn new(mut cx: Context, value: Handle<Value>) -> Self {
        let root = Rc::new(Root { value: UnsafeCell::new(*value), context: cx.identity() });
        cx.embedding.roots.push(Rc::downgrade(&root));
        Self(root)
    }

    pub fn get(&self, cx: Context) -> Handle<Value> {
        assert_eq!(self.0.context, cx.identity(), "persistent root belongs to another context");
        // The collector is the only writer and runs synchronously on the owning thread.
        unsafe { (*self.0.value.get()).to_handle(cx) }
    }
}

pub type ModuleCompletion =
    Box<dyn FnOnce(Context, EvalResult<DynModule>) -> super::alloc_error::AllocResult<()>>;
pub type ModuleLoader = Rc<
    dyn Fn(
        Context,
        Handle<FlatString>,
        ModuleRequest,
        Handle<Realm>,
        ModuleCompletion,
    ) -> super::alloc_error::AllocResult<()>,
>;
pub type ImportMeta = Rc<
    dyn Fn(
        Context,
        Handle<FlatString>,
        Handle<crate::runtime::object_value::ObjectValue>,
    ) -> crate::runtime::alloc_error::AllocResult<()>,
>;

#[derive(Default)]
pub struct Embedding {
    roots: Vec<Weak<Root>>,
    pub host_data: Option<Rc<dyn std::any::Any>>,
    pub module_loader: Option<ModuleLoader>,
    pub import_meta: Option<ImportMeta>,
    pub termination_flag: Option<Arc<AtomicBool>>,
}

impl Embedding {
    pub(crate) fn visit_roots(&mut self, visitor: &mut impl HeapVisitor) {
        self.roots.retain(|root| {
            if let Some(root) = root.upgrade() {
                unsafe {
                    visitor.visit_value(&mut *root.value.get());
                }
                true
            } else {
                false
            }
        });
    }

    #[inline]
    pub fn is_terminated(&self) -> bool {
        self.termination_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::wtf_8::Wtf8String,
        parser::source::Source,
        runtime::{
            ContextBuilder,
            gc::{GcType, HandleScopeGuard, Heap},
        },
    };

    #[test]
    fn persistent_root_survives_scope_exit_collection_and_heap_growth() {
        let cx = ContextBuilder::new().build().unwrap();
        cx.execute_then_drop(|mut cx| {
            let root = {
                let _scope = HandleScopeGuard::new(cx);
                let value = cx
                    .alloc_string("held by host")
                    .unwrap_or_else(|_| panic!("allocation failed"));
                Persistent::new(cx, value.into())
            };
            Heap::run_gc(cx, GcType::Normal);
            Heap::run_gc(cx, GcType::Grow { alloc_size: None });
            assert_eq!(root.get(cx).as_string().format().unwrap(), "held by host");
            drop(root);
            Heap::run_gc(cx, GcType::Normal);
            assert!(cx.embedding.roots.is_empty());
        });
    }

    #[test]
    fn bounded_tasks_leave_remaining_jobs_queued() {
        let cx = ContextBuilder::new().build().unwrap();
        cx.execute_then_drop(|mut cx| {
            let _scope = HandleScopeGuard::new(cx);
            let source = Rc::new(Source::new_for_string("test.js", Wtf8String::from_string("let count=0;for(let i=0;i<20;i++)Promise.resolve().then(()=>count++);".into())).unwrap());
            let pcx = crate::parser::ParseContext::new(source);
            let parsed = crate::parser::parse_script(&pcx, cx.options.clone()).unwrap_or_else(|_| panic!("parse failed"));
            let analyzed = crate::parser::analyze::analyze(parsed).unwrap_or_else(|_| panic!("analysis failed"));
            let script = crate::runtime::bytecode::generator::BytecodeProgramGenerator::generate_from_parse_script_result(cx, &analyzed, cx.initial_realm()).unwrap_or_else(|_| panic!("compile failed"));
            cx.with_initial_realm_stack_frame(cx.initial_realm_ptr(), |mut cx| cx.vm().execute_script(script)).unwrap_or_else(|_| panic!("script failed"));
            assert!(cx.task_queue().pending());
            assert_eq!(cx.run_tasks(0).unwrap_or_else(|_| panic!("tasks failed")), 0);
            assert_eq!(cx.run_tasks(3).unwrap_or_else(|_| panic!("tasks failed")), 3);
            assert!(cx.task_queue().pending());
            Heap::run_gc(cx, GcType::Normal);
            cx.run_all_tasks().unwrap_or_else(|_| panic!("tasks failed"));
            assert!(!cx.task_queue().pending());
        });
    }
}
