use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr as _;

use cargo_lock::package::GitReference;
use cargo_lock::package::SourceKind;
use cargo_lock::Lockfile;

use quote::ToTokens;
const JSON_RPC_METHODS_RS: &str = "openrpc_methods/stub-methods.rs";
//const JSON_RPC_METHODS_RS: &str = "src/components/json_rpc/methods.rs";

fn main() -> io::Result<()> {
    // Fetch the commit hash
    let commit_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to get commit hash")
        .stdout;
    let commit_hash = String::from_utf8(commit_hash).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=GIT_COMMIT={}", commit_hash.trim());

    // Fetch the current branch
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("Failed to get branch")
        .stdout;
    let branch = String::from_utf8(branch).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=BRANCH={}", branch.trim());

    // Set the build date
    let build_date = Command::new("date")
        .output()
        .expect("Failed to get build date")
        .stdout;
    let build_date = String::from_utf8(build_date).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=BUILD_DATE={}", build_date.trim());

    // Set the build user
    let build_user = whoami::username();
    println!("cargo:rustc-env=BUILD_USER={build_user}");

    // Set the version from Cargo.toml
    let version = env::var("CARGO_PKG_VERSION").expect("Failed to get version from Cargo.toml");
    println!("cargo:rustc-env=VERSION={version}");
    let lockfile = Lockfile::load("../Cargo.lock").expect("build script cannot load lockfile");
    let maybe_zebra_rev = lockfile.packages.iter().find_map(|package| {
        if package.name == cargo_lock::Name::from_str("zebra-chain").unwrap() {
            package
                .source
                .as_ref()
                .and_then(|source_id| match source_id.kind() {
                    SourceKind::Git(GitReference::Rev(rev)) => Some(rev),
                    _ => None,
                })
        } else {
            None
        }
    });
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("zebraversion.rs");
    fs::write(
        &dest_path,
        format!("const ZEBRA_VERSION: Option<&'static str> = {maybe_zebra_rev:?};"),
    )
    .unwrap();

    let out_dir_p = PathBuf::from(out_dir);
    // ? doesn't work here
    // because main is io::Result
    generate_rpc_openrpc(&out_dir_p)
        .expect("generate_rpc_openrpc to return OK instead of std::error::Error");

    Ok(())
}

// taken from zallet `main` @ 3a36f3d
fn generate_rpc_openrpc(out_dir_p: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("yep");
    eprintln!("eyep");
    // Parse the source file containing the `Rpc` trait.
    let methods_rs = fs::read_to_string(JSON_RPC_METHODS_RS)?;
    println!("YEPPPP made methods_rs");
    eprintln!("EYEPPP made methods_rs");
    let methods_ast = syn::parse_file(&methods_rs)?;

    let rpc_trait = methods_ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Trait(item_trait) if item_trait.ident == "Rpc" => Some(item_trait),
            _ => None,
        })
        .expect("present");

    let mut contents = "#[allow(unused_qualifications)]
pub(super) static METHODS: ::phf::Map<&str, RpcMethod> = ::phf::phf_map! {
"
    .to_string();

    for item in &rpc_trait.items {
        if let syn::TraitItem::Fn(method) = item {
            // Find methods via their `#[method(name = "command")]` attribute.
            let mut command = None;
            method
                .attrs
                .iter()
                .find(|attr| attr.path().is_ident("method"))
                .and_then(|attr| {
                    attr.parse_nested_meta(|meta| {
                        command = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                        Ok(())
                    })
                    .ok()
                });

            if let Some(command) = command {
                let module = match &method.sig.output {
                    syn::ReturnType::Type(_, ret) => match ret.as_ref() {
                        syn::Type::Path(type_path) => type_path.path.segments.first(),
                        _ => None,
                    },
                    _ => None,
                }
                .expect("required")
                .ident
                .to_string();

                let params = method.sig.inputs.iter().filter_map(|arg| match arg {
                    syn::FnArg::Receiver(_) => None,
                    syn::FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
                        syn::Pat::Ident(pat_ident) => {
                            let parameter = pat_ident.ident.to_string();
                            let rust_ty = pat_type.ty.as_ref();

                            // If we can determine the parameter's optionality, do so.
                            let (param_ty, required) = match rust_ty {
                                syn::Type::Path(type_path) => {
                                    let is_standalone_ident =
                                        type_path.path.leading_colon.is_none()
                                            && type_path.path.segments.len() == 1;
                                    let first_segment = &type_path.path.segments[0];

                                    if first_segment.ident == "Option" && is_standalone_ident {
                                        // Strip the `Option<_>` for the schema type.
                                        let schema_ty = match &first_segment.arguments {
                                            syn::PathArguments::AngleBracketed(args) => {
                                                match args.args.first().expect("valid Option") {
                                                    syn::GenericArgument::Type(ty) => ty,
                                                    _ => panic!("Invalid Option"),
                                                }
                                            }
                                            _ => panic!("Invalid Option"),
                                        };
                                        (schema_ty, Some(false))
                                    } else if first_segment.ident == "Vec" {
                                        // We don't know whether the vec may be empty.
                                        (rust_ty, None)
                                    } else {
                                        (rust_ty, Some(true))
                                    }
                                }
                                _ => (rust_ty, Some(true)),
                            };

                            // Handle a few conversions we know we need.
                            let param_ty = param_ty.to_token_stream().to_string();
                            let schema_ty = match param_ty.as_str() {
                                "age :: secrecy :: SecretString" => "String".into(),
                                _ => param_ty,
                            };

                            Some((parameter, schema_ty, required))
                        }
                        _ => None,
                    },
                });

                contents.push('"');
                contents.push_str(&command);
                contents.push_str("\" => RpcMethod {\n");

                contents.push_str("    description: \"");
                for attr in method
                    .attrs
                    .iter()
                    .filter(|attr| attr.path().is_ident("doc"))
                {
                    if let syn::Meta::NameValue(doc_line) = &attr.meta {
                        if let syn::Expr::Lit(docs) = &doc_line.value {
                            if let syn::Lit::Str(s) = &docs.lit {
                                // Trim the leading space from the doc comment line.
                                let line = s.value();
                                let trimmed_line = if line.is_empty() { &line } else { &line[1..] };

                                let escaped = trimmed_line.escape_default().collect::<String>();

                                contents.push_str(&escaped);
                                contents.push_str("\\n");
                            }
                        }
                    }
                }
                contents.push_str("\",\n");

                contents.push_str("    params: |_g| vec![\n");
                for (parameter, schema_ty, required) in params {
                    let param_upper = parameter.to_uppercase();

                    contents.push_str("        _g.param::<");
                    contents.push_str(&schema_ty);
                    contents.push_str(">(\"");
                    contents.push_str(&parameter);
                    contents.push_str("\", super::");
                    contents.push_str(&module);
                    contents.push_str("::PARAM_");
                    contents.push_str(&param_upper);
                    contents.push_str("_DESC, ");
                    match required {
                        Some(required) => contents.push_str(&required.to_string()),
                        None => {
                            // Require a helper const to be present.
                            contents.push_str("super::");
                            contents.push_str(&module);
                            contents.push_str("::PARAM_");
                            contents.push_str(&param_upper);
                            contents.push_str("_REQUIRED");
                        }
                    }
                    contents.push_str("),\n");
                }
                contents.push_str("    ],\n");

                contents.push_str("    result: |g| g.result::<super::");
                contents.push_str(&module);
                contents.push_str("::ResultType>(\"");
                contents.push_str(&command);
                contents.push_str("_result\"),\n");

                contents.push_str("    deprecated: ");
                contents.push_str(
                    &method
                        .attrs
                        .iter()
                        .any(|attr| attr.path().is_ident("deprecated"))
                        .to_string(),
                );
                contents.push_str(",\n");

                contents.push_str("},\n");
            }
        }
    }

    contents.push_str("};");

    let rpc_openrpc_path = out_dir_p.join("rpc_openrpc.rs");
    fs::write(&rpc_openrpc_path, contents)?;

    Ok(())
}
