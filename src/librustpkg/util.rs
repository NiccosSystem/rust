// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::libc;
use std::os;
use extra::workcache;
use rustc::driver::{driver, session};
use extra::getopts::groups::getopts;
use syntax::ast_util::*;
use syntax::codemap::{dummy_sp, Spanned};
use syntax::ext::base::ExtCtxt;
use syntax::{ast, attr, codemap, diagnostic, fold};
use syntax::attr::AttrMetaMethods;
use rustc::back::link;
use rustc::driver::session::{lib_crate, bin_crate};
use context::{in_target, StopBefore, Link, Assemble, BuildContext};
use package_id::PkgId;
use package_source::PkgSrc;
use workspace::pkg_parent_workspaces;
use path_util::{installed_library_in_workspace, U_RWX, rust_path, system_library, target_build_dir};
use messages::error;

pub use target::{OutputType, Main, Lib, Bench, Test, JustOne, lib_name_of, lib_crate_filename};
use workcache_support::{digest_file_with_date, digest_only_date};

// It would be nice to have the list of commands in just one place -- for example,
// you could update the match in rustpkg.rc but forget to update this list. I think
// that should be fixed.
static COMMANDS: &'static [&'static str] =
    &["build", "clean", "do", "info", "init", "install", "list", "prefer", "test", "uninstall",
      "unprefer"];


pub type ExitCode = int; // For now

pub struct Pkg {
    id: PkgId,
    bins: ~[~str],
    libs: ~[~str],
}

impl ToStr for Pkg {
    fn to_str(&self) -> ~str {
        self.id.to_str()
    }
}

pub fn is_cmd(cmd: &str) -> bool {
    COMMANDS.iter().any(|&c| c == cmd)
}

struct ListenerFn {
    cmds: ~[~str],
    span: codemap::Span,
    path: ~[ast::Ident]
}

struct ReadyCtx {
    sess: session::Session,
    crate: @ast::Crate,
    ext_cx: @ExtCtxt,
    path: ~[ast::Ident],
    fns: ~[ListenerFn]
}

fn fold_mod(_ctx: @mut ReadyCtx,
            m: &ast::_mod,
            fold: @fold::ast_fold) -> ast::_mod {
    fn strip_main(item: @ast::item) -> @ast::item {
        @ast::item {
            attrs: do item.attrs.iter().filter_map |attr| {
                if "main" != attr.name() {
                    Some(*attr)
                } else {
                    None
                }
            }.collect(),
            .. (*item).clone()
        }
    }

    fold::noop_fold_mod(&ast::_mod {
        items: do m.items.map |item| {
            strip_main(*item)
        },
        .. (*m).clone()
    }, fold)
}

fn fold_item(ctx: @mut ReadyCtx,
             item: @ast::item,
             fold: @fold::ast_fold) -> Option<@ast::item> {
    ctx.path.push(item.ident);

    let mut cmds = ~[];
    let mut had_pkg_do = false;

    for attr in item.attrs.iter() {
        if "pkg_do" == attr.name() {
            had_pkg_do = true;
            match attr.node.value.node {
                ast::MetaList(_, ref mis) => {
                    for mi in mis.iter() {
                        match mi.node {
                            ast::MetaWord(cmd) => cmds.push(cmd.to_owned()),
                            _ => {}
                        };
                    }
                }
                _ => cmds.push(~"build")
            }
        }
    }

    if had_pkg_do {
        ctx.fns.push(ListenerFn {
            cmds: cmds,
            span: item.span,
            path: /*bad*/ctx.path.clone()
        });
    }

    let res = fold::noop_fold_item(item, fold);

    ctx.path.pop();

    res
}

/// Generate/filter main function, add the list of commands, etc.
pub fn ready_crate(sess: session::Session,
                   crate: @ast::Crate) -> @ast::Crate {
    let ctx = @mut ReadyCtx {
        sess: sess,
        crate: crate,
        ext_cx: ExtCtxt::new(sess.parse_sess, sess.opts.cfg.clone()),
        path: ~[],
        fns: ~[]
    };
    let precursor = @fold::AstFoldFns {
        // fold_crate: fold::wrap(|a, b| fold_crate(ctx, a, b)),
        fold_item: |a, b| fold_item(ctx, a, b),
        fold_mod: |a, b| fold_mod(ctx, a, b),
        .. *fold::default_ast_fold()
    };

    let fold = fold::make_fold(precursor);

    @fold.fold_crate(crate)
}

pub fn compile_input(context: &BuildContext,
                     exec: &mut workcache::Exec,
                     pkg_id: &PkgId,
                     in_file: &Path,
                     workspace: &Path,
                     flags: &[~str],
                     cfgs: &[~str],
                     opt: bool,
                     what: OutputType) -> Option<Path> {
    assert!(in_file.components.len() > 1);
    let input = driver::file_input((*in_file).clone());
    debug!("compile_input: %s / %?", in_file.to_str(), what);
    // tjc: by default, use the package ID name as the link name
    // not sure if we should support anything else

    let out_dir = target_build_dir(workspace).push_rel(&pkg_id.path);
    // Make the output directory if it doesn't exist already
    assert!(os::mkdir_recursive(&out_dir, U_RWX));

    let binary = os::args()[0].to_managed();

    debug!("flags: %s", flags.connect(" "));
    debug!("cfgs: %s", cfgs.connect(" "));
    debug!("compile_input's sysroot = %s", context.sysroot().to_str());

    let crate_type = match what {
        Lib => lib_crate,
        Test | Bench | Main => bin_crate
    };
    let matches = getopts(debug_flags()
                          + match what {
                              Lib => ~[~"--lib"],
                              // --test compiles both #[test] and #[bench] fns
                              Test | Bench => ~[~"--test"],
                              Main => ~[]
                          }
                          + flags
                          + context.flag_strs()
                          + cfgs.flat_map(|c| { ~[~"--cfg", (*c).clone()] }),
                          driver::optgroups()).unwrap();
    debug!("rustc flags: %?", matches);

    // Hack so that rustpkg can run either out of a rustc target dir,
    // or the host dir
    let sysroot_to_use = @if !in_target(&context.sysroot()) {
        context.sysroot()
    }
    else {
        context.sysroot().pop().pop().pop()
    };
    debug!("compile_input's sysroot = %s", context.sysroot().to_str());
    debug!("sysroot_to_use = %s", sysroot_to_use.to_str());

    let output_type = match context.compile_upto() {
        Assemble => link::output_type_assembly,
        Link     => link::output_type_object,
        Pretty | Trans | Analysis => link::output_type_none,
        LLVMAssemble => link::output_type_llvm_assembly,
        LLVMCompileBitcode => link::output_type_bitcode,
        Nothing => link::output_type_exe
    };

    let options = @session::options {
        crate_type: crate_type,
        optimize: if opt { session::Aggressive } else { session::No },
        test: what == Test || what == Bench,
        maybe_sysroot: Some(sysroot_to_use),
        addl_lib_search_paths: @mut (~[]),
        output_type: output_type,
        .. (*driver::build_session_options(binary, &matches, diagnostic::emit)).clone()
    };

    let addl_lib_search_paths = @mut options.addl_lib_search_paths;
    // Make sure all the library directories actually exist, since the linker will complain
    // otherwise
    for p in addl_lib_search_paths.iter() {
        if os::path_exists(p) {
            assert!(os::path_is_dir(p));
        }
        else {
            assert!(os::mkdir_recursive(p, U_RWX));
        }
    }

    let sess = driver::build_session(options, diagnostic::emit);

    // Infer dependencies that rustpkg needs to build, by scanning for
    // `extern mod` directives.
    let cfg = driver::build_configuration(sess);
    let mut crate = driver::phase_1_parse_input(sess, cfg.clone(), &input);
    crate = driver::phase_2_configure_and_expand(sess, cfg.clone(), crate);

    find_and_install_dependencies(context, pkg_id, sess, exec, crate,
                                  |p| {
                                      debug!("a dependency: %s", p.to_str());
                                      // Pass the directory containing a dependency
                                      // as an additional lib search path
                                      if !addl_lib_search_paths.contains(&p) {
                                          // Might be inefficient, but this set probably
                                          // won't get too large -- tjc
                                          addl_lib_search_paths.push(p);
                                      }
                                  });

    // Inject the link attributes so we get the right package name and version
    if attr::find_linkage_metas(crate.attrs).is_empty() {
        let name_to_use = match what {
            Test  => fmt!("%stest", pkg_id.short_name).to_managed(),
            Bench => fmt!("%sbench", pkg_id.short_name).to_managed(),
            _     => pkg_id.short_name.to_managed()
        };
        debug!("Injecting link name: %s", name_to_use);
        let link_options =
            ~[attr::mk_name_value_item_str(@"name", name_to_use),
              attr::mk_name_value_item_str(@"vers", pkg_id.version.to_str().to_managed())] +
            ~[attr::mk_name_value_item_str(@"package_id",
                                           pkg_id.path.to_str().to_managed())];

        debug!("link options: %?", link_options);
        crate = @ast::Crate {
            attrs: ~[attr::mk_attr(attr::mk_list_item(@"link", link_options))],
            .. (*crate).clone()
        }
    }

    debug!("calling compile_crate_from_input, workspace = %s,
           building_library = %?", out_dir.to_str(), sess.building_library);
    compile_crate_from_input(in_file, exec, context.compile_upto(), &out_dir, sess, crate)
}

// Should use workcache to avoid recompiling when not necessary
// Should also rename this to something better
// If crate_opt is present, then finish compilation. If it's None, then
// call compile_upto and return the crate
// also, too many arguments
pub fn compile_crate_from_input(input: &Path,
                                exec: &mut workcache::Exec,
                                stop_before: StopBefore,
 // should be of the form <workspace>/build/<pkg id's path>
                                out_dir: &Path,
                                sess: session::Session,
// Returns None if one of the flags that suppresses compilation output was
// given
                                crate: @ast::Crate) -> Option<Path> {
    debug!("Calling build_output_filenames with %s, building library? %?",
           out_dir.to_str(), sess.building_library);

    // bad copy
    debug!("out_dir = %s", out_dir.to_str());
    let outputs = driver::build_output_filenames(&driver::file_input(input.clone()),
                                                 &Some(out_dir.clone()), &None,
                                                 crate.attrs, sess);

    debug!("Outputs are out_filename: %s and obj_filename: %s and output type = %?",
           outputs.out_filename.to_str(),
           outputs.obj_filename.to_str(),
           sess.opts.output_type);
    debug!("additional libraries:");
    for lib in sess.opts.addl_lib_search_paths.iter() {
        debug!("an additional library: %s", lib.to_str());
    }
    let analysis = driver::phase_3_run_analysis_passes(sess, crate);
    if driver::stop_after_phase_3(sess) { return None; }
    let translation = driver::phase_4_translate_to_llvm(sess, crate,
                                                        &analysis,
                                                        outputs);
    driver::phase_5_run_llvm_passes(sess, &translation, outputs);
    // The second check shouldn't be necessary, but rustc seems to ignore
    // -c
    if driver::stop_after_phase_5(sess)
        || stop_before == Link || stop_before == Assemble { return Some(outputs.out_filename); }
    driver::phase_6_link_output(sess, &translation, outputs);

    // Register dependency on the source file
    exec.discover_input("file", input.to_str(), digest_file_with_date(input));

    debug!("Built %s, date = %?", outputs.out_filename.to_str(),
           datestamp(&outputs.out_filename));

    Some(outputs.out_filename)
}

#[cfg(windows)]
pub fn exe_suffix() -> ~str { ~".exe" }

#[cfg(target_os = "linux")]
#[cfg(target_os = "android")]
#[cfg(target_os = "freebsd")]
#[cfg(target_os = "macos")]
pub fn exe_suffix() -> ~str { ~"" }

// Called by build_crates
pub fn compile_crate(ctxt: &BuildContext,
                     exec: &mut workcache::Exec,
                     pkg_id: &PkgId,
                     crate: &Path, workspace: &Path,
                     flags: &[~str], cfgs: &[~str], opt: bool,
                     what: OutputType) -> Option<Path> {
    debug!("compile_crate: crate=%s, workspace=%s", crate.to_str(), workspace.to_str());
    debug!("compile_crate: short_name = %s, flags =...", pkg_id.to_str());
    for fl in flags.iter() {
        debug!("+++ %s", *fl);
    }
    compile_input(ctxt, exec, pkg_id, crate, workspace, flags, cfgs, opt, what)
}


/// Collect all `extern mod` directives in `c`, then
/// try to install their targets, failing if any target
/// can't be found.
pub fn find_and_install_dependencies(context: &BuildContext,
                                     parent: &PkgId,
                                     sess: session::Session,
                                     exec: &mut workcache::Exec,
                                     c: &ast::Crate,
                                     save: @fn(Path)
                                     ) {
    use conditions::nonexistent_package::cond;

    do c.each_view_item() |vi: &ast::view_item| {
        debug!("A view item!");
        match vi.node {
            // ignore metadata, I guess
            ast::view_item_extern_mod(lib_ident, path_opt, _, _) => {
                let lib_name = match path_opt {
                    Some(p) => p,
                    None => sess.str_of(lib_ident)
                };
                debug!("Finding and installing... %s", lib_name);
                // Check standard Rust library path first
                match system_library(&context.sysroot(), lib_name) {
                    Some(ref installed_path) => {
                        debug!("It exists: %s", installed_path.to_str());
                        // Say that [path for c] has a discovered dependency on
                        // installed_path
                        // For binary files, we only hash the datestamp, not the contents.
                        // I'm not sure what the right thing is.
                        // Now we know that this crate has a discovered dependency on
                        // installed_path
                        exec.discover_input("binary", installed_path.to_str(),
                                                      digest_only_date(installed_path));
                    }
                    None => {
                        // FIXME #8711: need to parse version out of path_opt
                        debug!("Trying to install library %s, rebuilding it",
                               lib_name.to_str());
                        // Try to install it
                        let pkg_id = PkgId::new(lib_name);
                        let workspaces = pkg_parent_workspaces(&context.context, &pkg_id);
                        let dep_workspace = if workspaces.is_empty() {
                            error(fmt!("Couldn't find package %s, which is needed by %s, \
                                            in any of the workspaces in the RUST_PATH (%?)",
                                            lib_name, parent.to_str(), rust_path()));
                            cond.raise((pkg_id.clone(), ~"Dependency not found"))
                        }
                        else {
                            workspaces[0]
                        };
                        let (outputs_disc, inputs_disc) =
                            context.install(PkgSrc::new(dep_workspace.clone(),
                                false, pkg_id), &JustOne(Path(lib_crate_filename)));
                        debug!("Installed %s, returned %? dependencies and \
                               %? transitive dependencies",
                               lib_name, outputs_disc.len(), inputs_disc.len());
                        for dep in outputs_disc.iter() {
                            debug!("Discovering a binary input: %s", dep.to_str());
                            exec.discover_input("binary", dep.to_str(),
                                                digest_only_date(dep));
                        }
                        for &(ref what, ref dep) in inputs_disc.iter() {
                            if *what == ~"file" {
                                exec.discover_input(*what, *dep,
                                                    digest_file_with_date(&Path(*dep)));
                            }
                            else if *what == ~"binary" {
                                exec.discover_input(*what, *dep,
                                                    digest_only_date(&Path(*dep)));
                            }
                            else {
                                fail!("Bad kind: %s", *what);
                            }
                        }
                        // Also, add an additional search path
                        debug!("Adding additional search path: %s", lib_name);
                        let installed_library =
                            installed_library_in_workspace(&Path(lib_name), &dep_workspace)
                                .expect(fmt!("rustpkg failed to install dependency %s",
                                              lib_name));
                        let install_dir = installed_library.pop();
                        debug!("Installed %s into %s [%?]", lib_name, install_dir.to_str(),
                               datestamp(&installed_library));
                        save(install_dir);
                    }
                }}
            // Ignore `use`s
            _ => ()
        }
        true
    };
}

pub fn mk_string_lit(s: @str) -> ast::lit {
    Spanned {
        node: ast::lit_str(s),
        span: dummy_sp()
    }
}

#[cfg(test)]
mod test {
    use super::is_cmd;

    #[test]
    fn test_is_cmd() {
        assert!(is_cmd("build"));
        assert!(is_cmd("clean"));
        assert!(is_cmd("do"));
        assert!(is_cmd("info"));
        assert!(is_cmd("install"));
        assert!(is_cmd("prefer"));
        assert!(is_cmd("test"));
        assert!(is_cmd("uninstall"));
        assert!(is_cmd("unprefer"));
    }

}

pub fn option_to_vec<T>(x: Option<T>) -> ~[T] {
    match x {
       Some(y) => ~[y],
       None    => ~[]
    }
}

// tjc: cheesy
fn debug_flags() -> ~[~str] { ~[] }
// static DEBUG_FLAGS: ~[~str] = ~[~"-Z", ~"time-passes"];


/// Returns the last-modified date as an Option
pub fn datestamp(p: &Path) -> Option<libc::time_t> {
    debug!("Scrutinizing datestamp for %s - does it exist? %?", p.to_str(), os::path_exists(p));
    let out = p.stat().map(|stat| stat.st_mtime);
    debug!("Date = %?", out);
    out.map(|t| { *t as libc::time_t })
}
