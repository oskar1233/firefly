use core::ops::ControlFlow;
use std::collections::{BTreeMap, BTreeSet};

use firefly_diagnostics::*;
use firefly_intern::Symbol;
use firefly_pass::Pass;
use firefly_syntax_base::{ApplicationMetadata, Deprecation, FunctionName};

use crate::ast::*;
use crate::visit::{self, VisitMut};

/// Verifies that all declared exports have matching definitions
pub struct VerifyExports {
    reporter: Reporter,
}
impl VerifyExports {
    pub fn new(reporter: Reporter) -> Self {
        Self { reporter }
    }
}
impl Pass for VerifyExports {
    type Input<'a> = &'a mut Module;
    type Output<'a> = &'a mut Module;

    fn run<'a>(&mut self, module: Self::Input<'a>) -> anyhow::Result<Self::Output<'a>> {
        use core::cell::OnceCell;

        // Only calculate similar functions if we have an invalid export, which should be rare
        let similar_functions = OnceCell::new();

        for export in module.exports.iter() {
            if !module.functions.contains_key(export.as_ref()) {
                // We need to calculate similar functions, so populate the set now
                let similar = similar_functions.get_or_init(|| {
                    let mut similar = Vec::new();
                    for (name, function) in module.functions.iter() {
                        similar.push(Span::new(function.span, name.to_string()));
                    }
                    similar
                });

                let name = export.to_string();
                let most_similar = similar
                    .iter()
                    .map(|f| (strsim::jaro_winkler(&name, &f).abs(), f))
                    .max_by(|(x_score, _), (ref y_score, _)| x_score.total_cmp(y_score))
                    .and_then(|(score, f)| if score < 0.85 { None } else { Some(f) });

                match most_similar {
                    None => {
                        let span = export.span();
                        self.reporter.show_error(
                            "invalid export",
                            &[(
                                span,
                                "the referenced function is not defined in this module",
                            )],
                        );
                    }
                    Some(f) => {
                        let span = export.span();
                        let msg = format!("maybe you meant to export {} instead?", &f);
                        self.reporter.show_error(
                            "invalid export",
                            &[
                                (
                                    span,
                                    "the referenced function is not defined in this module",
                                ),
                                (f.span(), msg.as_str()),
                            ],
                        );
                    }
                }
            }
        }

        Ok(module)
    }
}

/// Verifies that the on_load function exists, if -on_load is present
pub struct VerifyOnLoadFunctions {
    reporter: Reporter,
}
impl VerifyOnLoadFunctions {
    pub fn new(reporter: Reporter) -> Self {
        Self { reporter }
    }
}
impl Pass for VerifyOnLoadFunctions {
    type Input<'a> = &'a mut Module;
    type Output<'a> = &'a mut Module;

    fn run<'a>(&mut self, module: Self::Input<'a>) -> anyhow::Result<Self::Output<'a>> {
        if let Some(on_load_name) = module.on_load.as_ref() {
            if !module.functions.contains_key(on_load_name.as_ref()) {
                let span = on_load_name.span();
                self.reporter.show_error(
                    "invalid on_load function",
                    &[(span, "this function is not defined in this module")],
                );
            }
        }

        Ok(module)
    }
}

/// Like `VerifyExports`, but for `-nifs`; ensures all NIF declarations have a corresponding definition.
pub struct VerifyNifs {
    reporter: Reporter,
}
impl VerifyNifs {
    pub fn new(reporter: Reporter) -> Self {
        Self { reporter }
    }
}
impl Pass for VerifyNifs {
    type Input<'a> = &'a mut Module;
    type Output<'a> = &'a mut Module;

    fn run<'a>(&mut self, module: Self::Input<'a>) -> anyhow::Result<Self::Output<'a>> {
        for nif in module.nifs.iter() {
            match module.functions.get(nif.as_ref()) {
                None => {
                    let span = nif.span();
                    self.reporter.show_error(
                        "invalid -nif declaration",
                        &[(
                            span,
                            "the referenced function is not defined in this module",
                        )],
                    );
                }
                Some(fun) => {
                    if !fun.is_nif {
                        let span = fun.span;
                        self.reporter.show_error(
                            "misplaced -nif declaration",
                            &[(
                                span,
                                "expected -nif declaration to precede the function it references",
                            )],
                        );
                    }
                }
            }
        }

        Ok(module)
    }
}

/// Verifies that all declared type specs are associated with a function definition
pub struct VerifyTypeSpecs {
    reporter: Reporter,
}
impl VerifyTypeSpecs {
    pub fn new(reporter: Reporter) -> Self {
        Self { reporter }
    }
}
impl Pass for VerifyTypeSpecs {
    type Input<'a> = &'a mut Module;
    type Output<'a> = &'a mut Module;

    fn run<'a>(&mut self, module: Self::Input<'a>) -> anyhow::Result<Self::Output<'a>> {
        for (spec_name, spec) in module.specs.iter() {
            let local_spec_name = spec_name.to_local();
            if !module.functions.contains_key(&local_spec_name) {
                self.reporter.show_warning(
                    "type spec for undefined function",
                    &[(
                        spec.span,
                        "this type spec has no corresponding function definition",
                    )],
                );
            }
        }
        Ok(module)
    }
}

/// Verifies that the callee of local function calls is defined or imported, or is dynamic and thus not statically analyzable
///
/// Additionally, checks if the callee is known to be deprecated and raises appropriate diagnostics.
///
/// NOTE: We could extend this analysis to cover calls to other modules, since at the point this analysis is run, we have
/// access to the entire set of modules that was provided to the compiler, however this does not account for cases in which
/// we're only compiling a library and thus only a subset of the modules is known - we could make such analysis optional and
/// only perform it when the full set of modules is known.
pub struct VerifyCalls<'app> {
    reporter: Reporter,
    app: &'app ApplicationMetadata,
}
impl<'app> VerifyCalls<'app> {
    pub fn new(reporter: Reporter, app: &'app ApplicationMetadata) -> Self {
        Self { reporter, app }
    }
}
impl<'app> Pass for VerifyCalls<'app> {
    type Input<'a> = &'a mut Module;
    type Output<'a> = &'a mut Module;

    fn run<'a>(&mut self, module: Self::Input<'a>) -> anyhow::Result<Self::Output<'a>> {
        let module_name = module.name.name;
        let locals = module.functions.keys().copied().collect::<BTreeSet<_>>();
        let imports = module
            .imports
            .iter()
            .map(|(name, sig)| (*name, sig.mfa()))
            .collect::<BTreeMap<FunctionName, FunctionName>>();

        for (_, function) in module.functions.iter_mut() {
            let mut visitor = VerifyCallsVisitor {
                reporter: self.reporter.clone(),
                app: self.app,
                module: module_name,
                locals: &locals,
                imports: &imports,
            };
            visitor.visit_mut_function(function);
        }
        Ok(module)
    }
}

struct VerifyCallsVisitor<'a> {
    reporter: Reporter,
    app: &'a ApplicationMetadata,
    module: Symbol,
    locals: &'a BTreeSet<FunctionName>,
    imports: &'a BTreeMap<FunctionName, FunctionName>,
}
impl<'a> VisitMut<()> for VerifyCallsVisitor<'a> {
    fn visit_mut_apply(&mut self, apply: &mut Apply) -> ControlFlow<()> {
        for arg in apply.args.iter_mut() {
            let _ = visit::visit_mut_expr(self, arg);
        }
        let span = apply.span();
        let arity = apply.args.len() as u8;
        match apply.callee.as_ref() {
            Expr::Remote(Remote {
                span: rspan,
                module,
                function,
                ..
            }) => match (module.as_atom(), function.as_atom()) {
                (Some(m), Some(f)) if m.name == self.module => {
                    let name = FunctionName::new_local(f.name, arity);
                    if !self.locals.contains(&name) {
                        let message =
                            format!("the function {} is not defined in this module", &name);
                        self.reporter.show_error(
                            "reference to undefined function",
                            &[(*rspan, message.as_str())],
                        );
                    }
                    ControlFlow::Continue(())
                }
                (Some(m), Some(f)) => {
                    let name = FunctionName::new(m.name, f.name, arity);
                    match self.app.get_function_deprecation(&name) {
                        None => ControlFlow::Continue(()),
                        Some(Deprecation::Module { span: dspan, flag }) => {
                            let note = format!("this module will be deprecated {}", &flag);
                            self.reporter.show_warning(
                                "use of deprecated module",
                                &[
                                    (m.span, note.as_str()),
                                    (dspan, "deprecation declared here"),
                                ],
                            );
                            ControlFlow::Continue(())
                        }
                        Some(Deprecation::Function {
                            span: dspan, flag, ..
                        }) => {
                            let note = format!("this function will be deprecated {}", &flag);
                            self.reporter.show_warning(
                                "use of deprecated function",
                                &[
                                    (f.span, note.as_str()),
                                    (dspan, "deprecation declared here"),
                                ],
                            );
                            ControlFlow::Continue(())
                        }
                        // These deprecation types have all been converted to Deprecation::Function
                        Some(Deprecation::FunctionAnyArity { .. }) => unreachable!(),
                    }
                }
                (None, Some(f)) => {
                    let name = FunctionName::new_local(f.name, arity);
                    if !self.locals.contains(&name) {
                        match self.imports.get(&name) {
                            None => {
                                let message = format!(
                                    "the function {} is not defined or imported in this module",
                                    &name
                                );
                                self.reporter.show_error(
                                    "reference to undefined function",
                                    &[(f.span, message.as_str())],
                                );
                            }
                            Some(imported) => match self.app.get_function_deprecation(&imported) {
                                None => (),
                                Some(Deprecation::Module { span: dspan, flag }) => {
                                    let note =
                                        format!("this function will be deprecated {}", &flag);
                                    self.reporter.show_warning(
                                        "use of deprecated module",
                                        &[
                                            (f.span, note.as_str()),
                                            (dspan, "deprecation declared here"),
                                        ],
                                    );
                                }
                                Some(Deprecation::Function {
                                    span: dspan, flag, ..
                                }) => {
                                    let note =
                                        format!("this function will be deprecated {}", &flag);
                                    self.reporter.show_warning(
                                        "use of deprecated function",
                                        &[
                                            (f.span, note.as_str()),
                                            (dspan, "deprecation declared here"),
                                        ],
                                    );
                                }
                                // These deprecation types have all been converted to Deprecation::Function
                                Some(Deprecation::FunctionAnyArity { .. }) => unreachable!(),
                            },
                        }
                    }
                    ControlFlow::Continue(())
                }
                _ => ControlFlow::Continue(()),
            },
            Expr::FunctionVar(FunctionVar::Resolved(name)) => {
                if name.module == Some(self.module) {
                    let local_name = name.item.to_local();
                    if !self.locals.contains(&local_name) {
                        let message =
                            format!("the function {} is not defined in this module", &local_name);
                        self.reporter.show_error(
                            "reference to undefined function",
                            &[(name.span(), message.as_str())],
                        );
                    }
                } else {
                    match self.app.get_function_deprecation(&name) {
                        None => (),
                        Some(Deprecation::Module { span: dspan, flag }) => {
                            let note = format!("this function will be deprecated {}", &flag);
                            self.reporter.show_warning(
                                "use of deprecated module",
                                &[
                                    (name.span(), note.as_str()),
                                    (dspan, "deprecation declared here"),
                                ],
                            );
                        }
                        Some(Deprecation::Function {
                            span: dspan, flag, ..
                        }) => {
                            let note = format!("this function will be deprecated {}", &flag);
                            self.reporter.show_warning(
                                "use of deprecated function",
                                &[
                                    (name.span(), note.as_str()),
                                    (dspan, "deprecation declared here"),
                                ],
                            );
                        }
                        // These deprecation types have all been converted to Deprecation::Function
                        Some(Deprecation::FunctionAnyArity { .. }) => unreachable!(),
                    }
                }
                if name.arity > arity {
                    let message = format!(
                        "{} requires {} arguments, but only {} were provided",
                        name, name.arity, arity
                    );
                    self.reporter
                        .show_error("missing arguments", &[(span, message.as_str())]);
                } else if name.arity < arity {
                    let message = format!(
                        "{} only takes {} arguments, but {} were provided",
                        name, name.arity, arity
                    );
                    self.reporter
                        .show_error("too many arguments", &[(span, message.as_str())]);
                }
                ControlFlow::Continue(())
            }
            Expr::FunctionVar(FunctionVar::PartiallyResolved(name)) => {
                let local_name = FunctionName::new_local(name.function, arity);
                if !self.locals.contains(&local_name) {
                    match self.imports.get(&local_name) {
                        None => {
                            let message = format!(
                                "the function {} is not defined or imported in this module",
                                &local_name
                            );
                            self.reporter.show_error(
                                "reference to undefined function",
                                &[(span, message.as_str())],
                            );
                        }
                        Some(imported) => match self.app.get_function_deprecation(&imported) {
                            None => (),
                            Some(Deprecation::Module { span: dspan, flag }) => {
                                let note = format!("this module will be deprecated {}", &flag);
                                self.reporter.show_warning(
                                    "use of deprecated module",
                                    &[(span, note.as_str()), (dspan, "deprecation declared here")],
                                );
                            }
                            Some(Deprecation::Function {
                                span: dspan, flag, ..
                            }) => {
                                let note = format!("this function will be deprecated {}", &flag);
                                self.reporter.show_warning(
                                    "use of deprecated function",
                                    &[(span, note.as_str()), (dspan, "deprecation declared here")],
                                );
                            }
                            // These deprecation types have all been converted to Deprecation::Function
                            Some(Deprecation::FunctionAnyArity { .. }) => unreachable!(),
                        },
                    }
                }

                if name.arity > arity {
                    let message = format!(
                        "{} requires {} arguments, but only {} were provided",
                        name, name.arity, arity
                    );
                    self.reporter
                        .show_error("missing arguments", &[(span, message.as_str())]);
                } else if name.arity < arity {
                    let message = format!(
                        "{} only takes {} arguments, but {} were provided",
                        name, name.arity, arity
                    );
                    self.reporter
                        .show_error("too many arguments", &[(span, message.as_str())]);
                }
                ControlFlow::Continue(())
            }
            Expr::FunctionVar(FunctionVar::Unresolved(name)) => {
                if let Some(Name::Atom(m)) = name.module {
                    match self.app.get_module_deprecation(&m.name) {
                        Some(Deprecation::Module { span: dspan, flag }) => {
                            let note = format!("this module will be deprecated {}", &flag);
                            self.reporter.show_warning(
                                "use of deprecated module",
                                &[(span, note.as_str()), (dspan, "deprecation declared here")],
                            );
                        }
                        _ => (),
                    }
                }
                if name.module.is_none() {
                    if let Name::Atom(a) = name.function {
                        let name = FunctionName::new_local(a.name, arity);
                        if !self.locals.contains(&name) {
                            match self.imports.get(&name) {
                                None => {
                                    let message = format!(
                                        "the function {} is not defined or imported in this module",
                                        &name
                                    );
                                    self.reporter.show_error(
                                        "reference to undefined function",
                                        &[(span, message.as_str())],
                                    );
                                }
                                Some(imported) => {
                                    match self.app.get_function_deprecation(&imported) {
                                        None => (),
                                        Some(Deprecation::Module { span: dspan, flag }) => {
                                            let note =
                                                format!("this module will be deprecated {}", &flag);
                                            self.reporter.show_warning(
                                                "use of deprecated module",
                                                &[
                                                    (span, note.as_str()),
                                                    (dspan, "deprecation declared here"),
                                                ],
                                            );
                                        }
                                        Some(Deprecation::Function {
                                            span: dspan, flag, ..
                                        }) => {
                                            let note = format!(
                                                "this function will be deprecated {}",
                                                &flag
                                            );
                                            self.reporter.show_warning(
                                                "use of deprecated function",
                                                &[
                                                    (span, note.as_str()),
                                                    (dspan, "deprecation declared here"),
                                                ],
                                            );
                                        }
                                        // These deprecation types have all been converted to Deprecation::Function
                                        Some(Deprecation::FunctionAnyArity { .. }) => {
                                            unreachable!()
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                match name.arity {
                    Arity::Int(i) if i > arity => {
                        let message = format!(
                            "this call requires {} arguments, but only {} were provided",
                            i, arity
                        );
                        self.reporter
                            .show_error("missing arguments", &[(span, message.as_str())]);
                    }
                    Arity::Int(i) if i < arity => {
                        let message = format!(
                            "this call should only have {} arguments, but {} were provided",
                            i, arity
                        );
                        self.reporter
                            .show_error("too many arguments", &[(span, message.as_str())]);
                    }
                    _ => (),
                }
                ControlFlow::Continue(())
            }
            Expr::Literal(Literal::Atom(id)) => {
                let name = FunctionName::new_local(id.name, arity);
                if !self.locals.contains(&name) {
                    match self.imports.get(&name) {
                        None => {
                            let message =
                                format!("{} is not defined or imported in this module", &name);
                            self.reporter.show_error(
                                "reference to undefined function",
                                &[(span, message.as_str())],
                            );
                        }
                        Some(imported) => match self.app.get_function_deprecation(&imported) {
                            None => (),
                            Some(Deprecation::Module { span: dspan, flag }) => {
                                let note = format!("this module will be deprecated {}", &flag);
                                self.reporter.show_warning(
                                    "use of deprecated module",
                                    &[(span, note.as_str()), (dspan, "deprecation declared here")],
                                );
                            }
                            Some(Deprecation::Function {
                                span: dspan, flag, ..
                            }) => {
                                let note = format!("this function will be deprecated {}", &flag);
                                self.reporter.show_warning(
                                    "use of deprecated function",
                                    &[(span, note.as_str()), (dspan, "deprecation declared here")],
                                );
                            }
                            // These deprecation types have all been converted to Deprecation::Function
                            Some(Deprecation::FunctionAnyArity { .. }) => unreachable!(),
                        },
                    }
                }
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Continue(()),
        }
    }
}
