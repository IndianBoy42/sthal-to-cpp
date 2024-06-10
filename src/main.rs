#![feature(try_blocks)]
#![feature(let_chains)]
#![warn(clippy::pedantic, clippy::perf)]
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Error, Result};
use clang::sonar::{find_functions, find_structs, find_typedefs};
use clang::{sonar, CompilationDatabase, Index};
use clang::{Clang, Parser};
use clap::Parser as ClapParser;
use convert_case::{Case, Casing};
use glob::glob;
use itertools::{chain, Itertools};
use lang_c::driver::{parse, Config};
use rayon::prelude::*;

#[derive(ClapParser, Debug)] // requires `derive` feature
#[command(term_width = 0)] // Just to make testing across clap features easier
struct Args {
    compiler: PathBuf,
    input: String,
    #[arg(default_value = ".")]
    outdir: PathBuf,
}

fn main() -> Result<()> {
    println!("Hello, world!");
    let args = Args::parse();
    // TODO: find the generated handles from Core/
    // We can RAII the init function
    let files = chain!(
        glob(&(args.input.clone() + "/*/*hal*.c"))?,
        glob(&(args.input + "/*/*ll*.h"))?
    );
    let clang = Clang::new().expect("Unable to initialize clang");
    let index = Index::new(&clang, false, false);
    let db = CompilationDatabase::from_directory(args.compiler)
        .ok()
        .context("Could not get db")?;
    for file in files {
        let res = parse_file(&index, &db, file, &args.outdir);
        match res {
            Ok(msg) => eprintln!("[OK] {msg}"),
            Err(e) => {
                eprintln!("{e}");
                // eprintln!("{e}", e = e.backtrace());
            }
        }
    }

    Ok(())
}

fn parse_file(
    index: &Index,
    db: &CompilationDatabase,
    file: Result<std::path::PathBuf, glob::GlobError>,
    outdir: &Path,
) -> Result<String> {
    let file = file?;
    println!("{}", file.display());

    let ofname = &file
        .file_name()
        .context("Invalid filename")?
        .to_str()
        .context("Non-utf-8 filename")?;
    let ofname = ofname
        .strip_suffix(".c")
        .or_else(|| ofname.strip_suffix(".h"))
        .context("Wrong extension")?;

    let Some((stver, fname)) = ofname.split_once('_') else {
        bail!("Invalid file name {ofname}")
    };
    let Some((hal_type, periph_type)) = fname.split_once('_') else {
        bail!("Invalid file name {fname}")
    };
    if periph_type.ends_with("_ex") {
        bail!("Skipping extension module, already processed in main module")
    }
    if !["hal", "ll"].contains(&hal_type) {
        bail!("Invalid hal_type {fname}");
    }

    let hdr = parse_header(index, db, &file).context("Could not parse the file")?;
    // dbg!(hdr.get_diagnostics());
    let functions = find_functions(hdr.get_entity().get_children()).collect_vec();

    let handle_types = find_handle_types(hal_type, &hdr, periph_type, &functions);

    let gen_code = generate_code(handle_types, ofname, periph_type, &functions, hal_type)?;

    let new_file = outdir.join(fname).with_extension("hpp");
    {
        let file = File::create(&new_file).context("Could not create new file")?;
        let mut file = BufWriter::new(file);
        file.write_all(gen_code.as_bytes())?;
    }

    Ok(format!(
        "{} converted to {}",
        file.display(),
        new_file.display()
    ))
}

fn generate_code(
    handle_types: Vec<String>,
    ofname: &str,
    periph_type: &str,
    functions: &[sonar::Declaration],
    hal_type: &str,
) -> Result<String, Error> {
    use std::fmt::Write;
    let mut code = String::new();
    writeln!(code, "#pragma once")?;
    writeln!(code, "#include \"{ofname}.h\"")?;
    writeln!(code, "namespace {hal_type} {{")?;
    if handle_types.is_empty() {
        let cname = periph_type.to_case(Case::Pascal);
        writeln!(code, "namespace {cname} {{")?;
        code.extend(static_functions(functions, hal_type, periph_type));
        writeln!(code, "}};")?;
    } else {
        for handle_type in handle_types {
            let Some((cname, _)) = handle_type.rsplit_once('_') else {
                eprintln!("Weird handle type {handle_type}");
                continue;
            };
            let cname = cname.to_case(Case::Pascal);
            // TODO: version that extends the struct rather than storing a handle
            writeln!(code, "class {cname} {{")?;
            writeln!(code, "public:")?;
            writeln!(code, "{handle_type} handle;")?;
            writeln!(
                code,
                "{cname}({handle_type} _handle) : handle(_handle) {{}}"
            )?;
            code.extend(handle_functions(
                functions,
                &handle_type,
                hal_type,
                periph_type,
            ));
            writeln!(code, "}};")?;
        }
    }
    writeln!(code, "}};")?;
    Ok(code)
}

fn find_handle_types(
    hal_type: &str,
    hdr: &clang::TranslationUnit,
    periph_type: &str,
    functions: &[sonar::Declaration],
) -> Vec<String> {
    let handle_types = if hal_type == "hal" {
        find_structs(hdr.get_entity().get_children())
            .map(|decl| decl.name)
            .filter(|decl| decl.ends_with("_HandleTypeDef"))
            .filter(|decl| decl.to_lowercase().contains(&periph_type.to_lowercase()))
            .filter(|decl| !decl.contains("const"))
            .map(|decl| decl + " *")
            .collect_vec()
    } else if hal_type == "ll" {
        functions
            .iter()
            .filter(|decl| decl.name.contains(&periph_type.to_uppercase()))
            .filter_map(|decl| try {
                decl.entity
                    .get_arguments()
                    .expect("known function")
                    .first()?
                    .get_type()
                    .expect("args have types")
                    .get_display_name()
            })
            .filter(|name| name.contains("_TypeDef"))
            // .filter(|decl| decl.contains(&periph_type.to_uppercase()))
            .filter(|decl| !decl.contains("const"))
            .unique()
            .collect_vec()
    } else {
        unreachable!("Unknown hal_type {hal_type}");
    };
    handle_types
}

fn handle_functions(
    functions: &[sonar::Declaration],
    handle_type: &str,
    hal_type: &str,
    periph: &str,
) -> Vec<String> {
    let is_ll = hal_type == "ll";
    let handle_type = handle_type.strip_prefix("__").unwrap_or(handle_type);
    let periph_up = &periph.to_uppercase();
    functions
        .iter()
        .filter(|decl| {
            (!is_ll && decl.name.starts_with("HAL_")) || (is_ll && decl.name.starts_with("LL_"))
        })
        .filter(|decl| !decl.name.ends_with("IRQHandler") && !decl.name.ends_with("Callback"))
        .rev()
        .map(|decl| {
            let code: Option<String> = try {
                let ret_type = decl
                    .entity
                    .get_result_type()
                    .expect("known function")
                    .get_display_name();
                let oname = &decl.name;
                let name = oname.split_once('_')?.1;
                let name = name.replace(&(periph_up.clone() + "_"), "");
                let name = &name.to_case(Case::Snake);
                let name = name.strip_prefix(periph).unwrap_or(name);
                let name = name.strip_prefix("_").unwrap_or(name);
                let mut args = decl.entity.get_arguments().expect("known function");
                if args.is_empty() {
                    if oname.contains(periph_up) {
                return format!(
                    "\tstatic inline {ret_type} {name}() {{ return {oname}(); }}\n"
                )
                    }
                    return String::new();
                }
                let first = args[0];
                let (prefix, handle) = if first
                    .get_type()
                    .expect("args have types")
                    .get_display_name()
                    .contains(handle_type)
                {
                    args.remove(0);
                    ("", vec!["this->handle".into()])
                }
                else if oname.contains(periph_up) {
                    ("static ", vec![])
                }
                else {
                    return String::new();
                };
                let call_args = chain!(
                    handle,
                    args.iter()
                        .map(|arg| arg.get_name().expect("args have names"))
                )
                .join(", ");
                let args = args
                    .into_iter()
                    .map(|arg| arg.get_pretty_printer().print())
                    .join(", ");

                format!(
                    "\t{prefix}inline {ret_type} {name}({args}) {{ return {oname}({call_args}); }}\n"
                )
            };
            code.unwrap_or_default()
        })
        .collect_vec()
}

fn static_functions(functions: &[sonar::Declaration], hal_type: &str, periph: &str) -> Vec<String> {
    let is_ll = hal_type == "ll";
    let periph_up = &periph.to_uppercase();
    functions
        .iter()
        .filter(|decl| {
            (!is_ll && decl.name.starts_with("HAL_")) || (is_ll && decl.name.starts_with("LL_"))
        })
        .filter(|decl| !decl.name.ends_with("IRQHandler") && !decl.name.ends_with("Callback"))
        .rev()
        .map(|decl| {
            let code: Option<String> = try {
                let ret_type = decl
                    .entity
                    .get_result_type()
                    .expect("known function")
                    .get_display_name();
                // TODO: convert StatusTypeDef into bool
                let oname = &decl.name;
                if !oname.contains(periph_up) {
                    return String::new();
                }
                let name = oname.split_once('_')?.1;
                let name = name.replace(&(periph_up.clone() + "_"), "");
                let name = &name.to_case(Case::Snake);
                let name = name.strip_prefix(periph).unwrap_or(name);
                let name = name.strip_prefix("_").unwrap_or(name);
                let args = decl.entity.get_arguments().expect("known function");
                let call_args = args
                    .iter()
                    .map(|arg| arg.get_name().expect("args have names"))
                    .join(", ");
                let args = args
                    .into_iter()
                    .map(|arg| arg.get_pretty_printer().print())
                    .join(", ");

                format!(
                    "\tstatic inline {ret_type} {name}({args}) {{ return {oname}({call_args}); }}\n"
                )
            };
            code.unwrap_or_default()
        })
        .collect_vec()
}

fn parse_header<'a>(
    index: &'a Index,
    db: &CompilationDatabase,
    file: &Path,
) -> std::prelude::v1::Result<clang::TranslationUnit<'a>, clang::SourceError> {
    let mut args = db
        .get_compile_commands(file)
        .ok()
        .and_then(|cc| {
            cc.get_commands()
                .first()
                .map(clang::CompileCommand::get_arguments)
        })
        .unwrap_or_default();
    args.retain(|arg| arg.starts_with("-D") || arg.starts_with("-I"));
    args.push("-D__STATIC_INLINE=".to_owned());
    args.push("-Dinline=".to_owned());
    index
        .parser(file)
        .skip_function_bodies(true)
        .arguments(&args)
        .keep_going(true)
        .incomplete(true)
        .parse()
}
