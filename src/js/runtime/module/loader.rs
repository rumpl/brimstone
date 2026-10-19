use std::{collections::HashSet, path::Path, rc::Rc};

use crate::{
    completion_value, must_a,
    parser::{ParseContext, analyze::analyze, parse_module, print_program, source::Source},
    runtime::{
        Context, Handle, Realm, Value,
        abstract_operations::call_object,
        alloc_error::AllocResult,
        bytecode::generator::BytecodeProgramGenerator,
        context::ModuleCacheKey,
        error::{syntax_error, syntax_parse_error},
        eval_result::EvalResult,
        intrinsics::{intrinsics::Intrinsic, json_object::json_parse},
        module::{
            module::{DynModule, ModuleId},
            source_text_module::{ModuleRequest, ModuleState, SourceTextModule},
            synthetic_module::SyntheticModule,
        },
        promise_object::{PromiseCapability, PromiseObject},
        string_value::FlatString,
    },
};

/// A graph loader may outlive the current VM turn while the host fetches imports.
struct GraphLoader {
    is_loading: bool,
    pending: usize,
    visited: HashSet<ModuleId>,
    capability: crate::runtime::embedding::Persistent,
    realm: crate::runtime::embedding::Persistent,
}

type SharedLoader = Rc<std::cell::RefCell<GraphLoader>>;

impl GraphLoader {
    fn inner(loader: &SharedLoader, cx: Context, module: DynModule) -> AllocResult<()> {
        if !loader.borrow().is_loading {
            return Ok(());
        }
        if let Some(mut module) = module.as_source_text_module() {
            if matches!(module.state(), ModuleState::New | ModuleState::Unlinked)
                && loader.borrow_mut().visited.insert(module.id())
            {
                module.set_state(ModuleState::Unlinked);
                let requests = module.requested_modules();
                loader.borrow_mut().pending += requests.len();
                for i in 0..requests.len() {
                    // Reload arrays after each callback: callbacks may allocate and collect.
                    let loaded = module.loaded_modules().as_slice()[i];
                    if let Some(loaded) = loaded {
                        Self::inner(loader, cx, DynModule::from_heap(&loaded))?;
                    } else {
                        let request =
                            ModuleRequest::from_heap(&module.requested_modules().as_slice()[i]);
                        let realm = loader.borrow().realm.get(cx).cast::<Realm>();
                        let loader = loader.clone();
                        let root = crate::runtime::embedding::Persistent::new(cx, module.into());
                        request_imported_module(
                            cx,
                            module.source_file_path(),
                            request,
                            realm,
                            Box::new(move |cx, result| {
                                let mut module = root.get(cx).cast::<SourceTextModule>();
                                if let Ok(imported) = result {
                                    if !module.has_loaded_module_at(i) {
                                        module.set_loaded_module_at(i, imported);
                                    }
                                }
                                Self::finish(&loader, cx, result)
                            }),
                        )?;
                    }
                    if !loader.borrow().is_loading {
                        return Ok(());
                    }
                }
            }
        }
        let capability = {
            let mut state = loader.borrow_mut();
            state.pending -= 1;
            if state.pending != 0 {
                return Ok(());
            }
            state.is_loading = false;
            state.capability.get(cx).cast::<PromiseCapability>()
        };
        must_a!(call_object(cx, capability.resolve(), cx.undefined(), &[cx.undefined()]));
        Ok(())
    }

    fn finish(
        loader: &SharedLoader,
        cx: Context,
        result: EvalResult<DynModule>,
    ) -> AllocResult<()> {
        if !loader.borrow().is_loading {
            return Ok(());
        }
        match completion_value!(result) {
            Ok(module) => Self::inner(loader, cx, module),
            Err(error) => {
                let capability = {
                    let mut state = loader.borrow_mut();
                    state.is_loading = false;
                    state.capability.get(cx).cast::<PromiseCapability>()
                };
                must_a!(call_object(cx, capability.reject(), cx.undefined(), &[error]));
                Ok(())
            }
        }
    }
}

pub fn load_requested_modules(
    cx: Context,
    module: Handle<SourceTextModule>,
) -> AllocResult<Handle<PromiseObject>> {
    use crate::runtime::embedding::Persistent;
    let capability = must_a!(PromiseCapability::new(
        cx,
        cx.get_intrinsic(Intrinsic::PromiseConstructor).into()
    ));
    let loader = Rc::new(std::cell::RefCell::new(GraphLoader {
        is_loading: true,
        pending: 1,
        visited: HashSet::new(),
        capability: Persistent::new(cx, capability.into()),
        realm: Persistent::new(cx, module.program_function_ptr().realm().into()),
    }));
    GraphLoader::inner(&loader, cx, module.as_dyn_module())?;
    Ok(capability.promise().cast::<PromiseObject>())
}

pub fn request_imported_module(
    cx: Context,
    path: Handle<FlatString>,
    request: ModuleRequest,
    realm: Handle<Realm>,
    finish: crate::runtime::embedding::ModuleCompletion,
) -> AllocResult<()> {
    if let Some(loader) = cx.embedding.module_loader.clone() {
        loader(cx, path, request, realm, finish)
    } else {
        finish(cx, host_load_imported_module(cx, path, request, realm))
    }
}

/// HostLoadImportedModule (https://tc39.es/ecma262/#sec-HostLoadImportedModule)
pub fn host_load_imported_module(
    mut cx: Context,
    source_file_path: Handle<FlatString>,
    module_request: ModuleRequest,
    realm: Handle<Realm>,
) -> EvalResult<DynModule> {
    let source_file_path = Path::new(&source_file_path.to_string())
        .canonicalize()
        .unwrap();
    let source_file_dir = source_file_path.parent().unwrap();

    // Join source file path with specifier path so that relative specifiers will be applied to
    // source path and absolute specifiers will overwrite source path.
    let new_module_path = source_file_dir
        .join(Path::new(&module_request.specifier.to_string()))
        .canonicalize();

    let new_module_path = match new_module_path {
        Ok(path) => path,
        Err(error) => return syntax_error(cx, &error.to_string()),
    };

    let new_module_path_string = cx
        .alloc_string_ptr(new_module_path.to_str().unwrap())?
        .to_handle();
    let module_cache_key = ModuleCacheKey::new(new_module_path_string, module_request.attributes);

    // Use the cached module if it has already been loaded
    {
        if let Some(module) = cx.modules.get(&module_cache_key.into_heap()) {
            return Ok(DynModule::from_heap(module));
        }
    }

    // Check if the module is a JSON module
    if let Some(attributes) = &module_request.attributes {
        if attributes.has_attribute_with_value("type", "json") {
            let json_value = parse_json_file_at_path(cx, new_module_path.as_path())?;
            let json_module = SyntheticModule::new_default_export(cx, realm, json_value)?;

            // Cache the JSON module
            cx.insert_module(module_cache_key, json_module.as_dyn_module())?;

            return Ok(json_module.as_dyn_module());
        }
    }

    // Find the source file at the given path
    let source = match Source::new_from_file(new_module_path.to_str().unwrap()) {
        Ok(source) => Rc::new(source),
        Err(error) => return syntax_parse_error(cx, &error),
    };

    // Parse the source, returning AST
    let pcx = ParseContext::new(source);
    let parse_result = match parse_module(&pcx, cx.options.clone()) {
        Ok(parse_result) => parse_result,
        Err(error) => return syntax_parse_error(cx, &error),
    };

    if cx.options.print_ast {
        println!("{}", print_program(&parse_result));
    }

    // Analyze AST
    let analyzed_result = match analyze(parse_result) {
        Ok(analyzed_result) => analyzed_result,
        Err(parse_errors) => return syntax_parse_error(cx, &parse_errors.errors[0]),
    };

    // Finally generate the SourceTextModule for the parsed module
    let bytecode_result = BytecodeProgramGenerator::generate_from_parse_module_result(
        cx,
        &Rc::new(analyzed_result),
        realm,
    );

    let module = match bytecode_result {
        Ok(module) => module,
        Err(error) => return syntax_error(cx, &error.to_string()),
    };

    // Cache the module
    cx.insert_module(module_cache_key, module.as_dyn_module())?;

    Ok(module.as_dyn_module())
}

fn parse_json_file_at_path(mut cx: Context, path: &Path) -> EvalResult<Handle<Value>> {
    // Read the contents of the file into the heap
    let file_contents = match Source::new_from_file(path.to_str().unwrap()) {
        Ok(source) => cx.alloc_wtf8_string(&source.contents)?,
        Err(error) => return syntax_parse_error(cx, &error),
    };

    json_parse(cx, file_contents.as_value(), cx.undefined())
}
