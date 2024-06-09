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
                eprintln!("[ERR] {e}");
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
        bail!("This is an extension module")
    }

    let hdr = parse_header(index, db, &file).context("Could not parse the file")?;
    // dbg!(hdr.get_diagnostics());
    let functions = find_functions(hdr.get_entity().get_children()).collect_vec();

    let handle_type = if hal_type == "hal" {
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
        bail!("Invalid hal_type {fname}");
    };
    if handle_type.is_empty() {
        bail!("No handle type found")
        // TODO: just namespace the functions?
    };

    let class_code = {
        use std::fmt::Write;
        let mut class_code = String::new();
        writeln!(class_code, "#pragma once")?;
        writeln!(class_code, "#include \"{ofname}.h\"")?;
        writeln!(class_code, "namespace {hal_type} {{")?;
        for handle_type in handle_type {
            let Some(split_once) = handle_type.rsplit_once('_') else {
                continue;
            };
            let cname = split_once.0.to_case(Case::Pascal);
            // TODO: version that extends the struct rather than storing a handle
            writeln!(class_code, "class {cname} {{")?;
            writeln!(class_code, "public:")?;
            writeln!(class_code, "{handle_type} handle;")?;
            writeln!(
                class_code,
                "{cname}({handle_type} _handle) : handle(_handle) {{}}"
            )?;
            class_code.extend(body_functions(&functions, &handle_type, periph_type));
            writeln!(class_code, "}};")?;
        }
        writeln!(class_code, "}};")?;
        class_code
    };

    let new_file = outdir.join(fname).with_extension("hpp");
    {
        let file = File::create(&new_file).context("Could not create new file")?;
        let mut file = BufWriter::new(file);
        file.write_all(class_code.as_bytes())?;
    }

    Ok(format!(
        "{} converted to {}",
        file.display(),
        new_file.display()
    ))
}

fn body_functions(
    functions: &[sonar::Declaration],
    handle_type: &str,
    periph: &str,
) -> Vec<String> {
    let is_ll = !handle_type.contains("Handle");
    let handle_type = handle_type.strip_prefix("__").unwrap_or(handle_type);
    let periph_up = periph.to_uppercase();
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
                let name = oname.split_once('_')?.1;
                let name = name.strip_prefix(&periph_up).unwrap_or(name);
                let name = name.strip_prefix("_").unwrap_or(name);
                let name = &name.to_case(Case::Snake);
                let name = name.strip_prefix(periph).unwrap_or(name);
                let name = name.strip_prefix("_").unwrap_or(name);
                let mut args = decl.entity.get_arguments().expect("known function");
                if args.is_empty() {
                    if oname.contains(periph) {
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
                else if oname.contains(periph) {
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
