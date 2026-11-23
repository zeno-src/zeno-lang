//! Stage 12: the LLVM backend. It turns a checked program into LLVM IR.
//!
//! ```text
//! tokens -> parser -> tree -> checker -> [emit_llvm] -> LLVM IR -> clang -> program
//! ```
//!
//! LLVM is the compiler library behind Clang, Rust and Swift. Its input is
//! a small typed language called IR ("intermediate representation"). Hand
//! LLVM a program in IR and it does the optimising and writes the machine
//! code for whichever processor is asked for. So this stage does what
//! `emit_c` does, one level further down: there is no C compiler between
//! Zeno and LLVM, and nothing of C's rules to work around.
//!
//! IR reads like assembly for a machine that does not exist:
//!
//! ```text
//! define internal double @z_area(double %arg.w, double %arg.h) {
//! entry:
//!   %t1 = fmul double %arg.w, %arg.h
//!   ret double %t1
//! }
//! ```
//!
//! Four things about it shape everything in this file:
//!
//! 1. **Every value has a type, written at every use.** `i64` is a whole
//!    number of 64 bits, `double` a float, `i1` one bit (a bool), `ptr` an
//!    address. The checker's `Types` say which is which, as for the C
//!    backend.
//! 2. **There are no nested expressions.** `a + b * c` is two
//!    instructions, each giving a new name: `%t1 = fmul ...` then
//!    `%t2 = fadd ...`. The order things happen in is the order they are
//!    written, so Zeno's left-to-right rule costs nothing here. (In
//!    `emit_c` it took `sequence` and `settle`.)
//! 3. **A name is given a value once, and never again.** This is called
//!    SSA, "static single assignment". A Zeno `let` is exactly that, so a
//!    `let` *is* an IR value. A `var` changes, so it gets a place in
//!    memory (`alloca`), and is read with `load` and written with
//!    `store`. LLVM's first pass turns those back into plain values where
//!    it can. See `declare`.
//! 4. **There is no `if` and no `while`.** A function is a list of
//!    blocks. Each block has a label, runs straight through, and ends
//!    with exactly one jump: `br` to another block, or `ret`. Loops and
//!    branches are spelled out with those. See `Code`.
//!
//! The file is in this order: the runtime the program links to, the
//! pieces the emitter keeps, the file it writes, how one instruction is
//! written, then functions, classes and enums, statements, what a build
//! with no collector needs, owned values, loops, `match` and `try`,
//! expressions (with lists and values that may be `none`), calls, objects
//! and enum values, functions as values, and at the end how a value is
//! shown as text and compared with another.
//!
//! The compiled program still needs the runtime (`runtime.c`) for text,
//! printing, lists, errors and the collector. `zeno build --llvm` writes
//! it out as a second file and hands both to clang. See `runtime`.
//!
//! It builds the three ways the C backend does (`Build` in `emit_c`):
//! the ordinary way, `--firmware` (errors as values, nothing jumps) and
//! `--no-collector` (a firmware build with no collector in it).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::ast::*;
use crate::checker::{spot, FunctionKind, Kind, Pointee, Types};
use crate::emit_c::{self, is_boxed, is_text, is_written, Build};
use crate::error::Error;
use crate::ints::{self, Width};
use crate::test_build::TestBuild;
use crate::token::Span;
use crate::units;

// ---------------------------------------------------------------- The runtime

/// The runtime as an LLVM build needs it: the ordinary one, with its
/// functions made visible to another file.
///
/// A C build pastes the runtime in front of the program, and every
/// function in it is `static`, which in C means "only this file may use
/// it". That lets the C compiler drop the ones a program never calls.
/// Here the program is in a second file, of IR, so the word has to go.
/// Only a `static` at the start of a line is removed, which is one that
/// stands outside any function.
#[cfg(test)]
pub fn runtime(build: Build) -> String {
    runtime_for(build, false)
}

/// The same, saying whether the pool of a build with no collector grows
/// (see `emit_c::Options::grows`).
pub fn runtime_for(build: Build, grows: bool) -> String {
    let (firmware, collector) = (build != Build::Ordinary, build != Build::NoCollector);
    beside_ir(&emit_c::runtime_for(firmware, collector, grows && !collector), firmware)
}

/// The same, for `--memory` with no collector: a block that is given back
/// is filled, and looked at when it is handed out again
/// (`emit_c::with_spares_checked`, decision 56.15).
pub fn runtime_with_spares_checked(grows: bool) -> String {
    beside_ir(&emit_c::with_spares_checked(&emit_c::runtime_for(true, false, grows)), true)
}

/// The runtime of C as the file beside the IR has it.
fn beside_ir(runtime: &str, firmware: bool) -> String {
    // A function that never returns has that said in front of `static`.
    const NEVER: &str = "ZENO_NORETURN ";
    let mut text = String::new();
    for line in runtime.lines() {
        match line.strip_prefix(NEVER) {
            Some(rest) => {
                text += NEVER;
                text += rest.strip_prefix("static ").unwrap_or(rest);
            }
            None => text += line.strip_prefix("static ").unwrap_or(line),
        }
        text.push('\n');
    }
    // A firmware build has no `setjmp`, so it has no `zeno_try` either.
    if !firmware {
        text += FOR_TRY;
    }
    text
}

/// What follows the place in a call to `_setjmp`.
///
/// Everywhere but Windows, nothing: `_setjmp(place)`. Microsoft's
/// `_setjmp` takes a second thing, the frame of the function that calls
/// it, and its `longjmp` uses that to run the clean-up code of every
/// function it jumps over, which is how C++ and Windows' own errors
/// work. A Zeno program has no such code, and `null` there asks for a
/// plain jump, which is what every other system does. A C compiler adds
/// this without being asked, because `setjmp` in C is a macro. IR has no
/// macros, so it is written out.
const SETJMP_TAKES: &str = if cfg!(windows) { ", ptr null" } else { "" };

/// Three small functions added to the runtime for `try`.
///
/// A `try` needs a `zeno_try` on the stack, which holds a C `jmp_buf`:
/// the processor's registers, saved by `setjmp`. How big that is differs
/// from one processor and C library to the next, and only C knows. So the
/// IR asks for the size while the program runs, and leaves the fields to
/// these.
const FOR_TRY: &str = "
/* ------------------------------------------- For a build through LLVM IR */

size_t zeno_try_size(void) {
    return sizeof(zeno_try);
}

/* A `try` begins: from here on, errors come to `place`. */
void zeno_try_open(zeno_try *place) {
    place->outer = zeno_trying;
    zeno_trying = place;
}

/* It is over, with no error. */
void zeno_try_close(zeno_try *place) {
    zeno_trying = place->outer;
}
";

/// Everything in the runtime and the C library that the IR may call, as
/// LLVM wants it declared. Only the ones a program uses are written into
/// its file.
///
/// `signext` and `zeroext` say how a value narrower than a register is
/// widened when it is handed over. C compilers do this without being
/// asked. IR has to say it, or a processor such as RISC-V may read
/// leftover bits.
///
/// A list keeps each item in a slot, which in C is a union the size of
/// one register (`zeno_slot`). A union that small is handed over as a
/// plain number, so here a slot is an `i64`. See `to_slot`.
const DECLARED: &[(&str, &str)] = &[
    // The processor's own overflow flag, as LLVM exposes it: the result
    // and one bit that says whether it fitted.
    ("llvm.sadd.with.overflow.i64", "declare { i64, i1 } @llvm.sadd.with.overflow.i64(i64, i64)"),
    ("llvm.ssub.with.overflow.i64", "declare { i64, i1 } @llvm.ssub.with.overflow.i64(i64, i64)"),
    ("llvm.smul.with.overflow.i64", "declare { i64, i1 } @llvm.smul.with.overflow.i64(i64, i64)"),
    ("zeno_overflow", "declare void @zeno_overflow(i32 signext, i32 signext)"),
    ("zeno_by_zero", "declare void @zeno_by_zero(i32 signext, i32 signext)"),
    ("zeno_gone", "declare void @zeno_gone(i32 signext, i32 signext)"),
    ("zeno_pow", "declare i64 @zeno_pow(i64, i64, i32 signext, i32 signext)"),
    // The board. A pin is its number.
    ("zeno_pin", "declare i64 @zeno_pin(i64, i32 signext, i32 signext)"),
    ("zeno_pin_on", "declare void @zeno_pin_on(i64)"),
    ("zeno_pin_off", "declare void @zeno_pin_off(i64)"),
    ("zeno_pin_toggle", "declare void @zeno_pin_toggle(i64)"),
    ("zeno_pin_is_on", "declare zeroext i1 @zeno_pin_is_on(i64)"),
    ("zeno_str_pin", "declare ptr @zeno_str_pin(i64)"),
    ("zeno_input_pin", "declare i64 @zeno_input_pin(i64, ptr, i32 signext, i32 signext)"),
    ("zeno_input_is_high", "declare zeroext i1 @zeno_input_is_high(i64)"),
    ("zeno_input_is_low", "declare zeroext i1 @zeno_input_is_low(i64)"),
    ("zeno_str_input_pin", "declare ptr @zeno_str_input_pin(i64)"),
    ("zeno_pwm_pin", "declare i64 @zeno_pwm_pin(i64, double, i32 signext, i32 signext)"),
    ("zeno_pwm_duty", "declare void @zeno_pwm_duty(i64, double, i32 signext, i32 signext)"),
    ("zeno_pwm_pulse", "declare void @zeno_pwm_pulse(i64, double, i32 signext, i32 signext)"),
    ("zeno_str_pwm_pin", "declare ptr @zeno_str_pwm_pin(i64)"),
    ("zeno_interrupt", "declare i64 @zeno_interrupt(i64, ptr, i32 signext, i32 signext)"),
    ("zeno_interrupt_edges", "declare i64 @zeno_interrupt_edges(i64)"),
    ("zeno_interrupt_wait", "declare i64 @zeno_interrupt_wait(i64, i32 signext, i32 signext)"),
    ("zeno_str_interrupt", "declare ptr @zeno_str_interrupt(i64)"),
    ("zeno_ticker", "declare i64 @zeno_ticker(double, i32 signext, i32 signext)"),
    ("zeno_ticker_wait", "declare i64 @zeno_ticker_wait(i64)"),
    ("zeno_ticker_ticks", "declare i64 @zeno_ticker_ticks(i64)"),
    ("zeno_ticker_restart", "declare void @zeno_ticker_restart(i64)"),
    ("zeno_ticker_period", "declare double @zeno_ticker_period(i64)"),
    ("zeno_ticker_str", "declare ptr @zeno_ticker_str(i64)"),
    ("zeno_analog_pin", "declare i64 @zeno_analog_pin(i64, i32 signext, i32 signext)"),
    ("zeno_analog_max", "declare i64 @zeno_analog_max()"),
    ("zeno_analog_count", "declare i64 @zeno_analog_count(i64)"),
    ("zeno_analog_voltage", "declare double @zeno_analog_voltage(i64)"),
    ("zeno_str_analog_pin", "declare ptr @zeno_str_analog_pin(i64)"),
    ("zeno_serial", "declare i64 @zeno_serial(i64, double, i32 signext, i32 signext)"),
    ("zeno_serial_write", "declare void @zeno_serial_write(i64, ptr)"),
    ("zeno_serial_write_bytes", "declare void @zeno_serial_write_bytes(i64, ptr)"),
    ("zeno_serial_read_byte", "declare i64 @zeno_serial_read_byte(i64)"),
    ("zeno_serial_has_byte", "declare zeroext i1 @zeno_serial_has_byte(i64)"),
    ("zeno_str_serial", "declare ptr @zeno_str_serial(i64)"),
    // A register is its address, and the `i32` is how many bits it has.
    ("zeno_register_read", "declare i64 @zeno_register_read(i64, i32 signext)"),
    ("zeno_register_write", "declare void @zeno_register_write(i64, i32 signext, i64)"),
    ("zeno_str_register", "declare ptr @zeno_str_register(i64)"),
    // `allocate` and `free`: how many items, and the bytes in one.
    ("zeno_allocate", "declare ptr @zeno_allocate(i64, i64, i32 signext, i32 signext)"),
    ("zeno_release", "declare void @zeno_release(ptr)"),
    ("zeno_sleep", "declare void @zeno_sleep(double)"),
    ("zeno_now", "declare double @zeno_now()"),
    ("zeno_abs", "declare i64 @zeno_abs(i64, i32 signext, i32 signext)"),
    ("zeno_shl", "declare i64 @zeno_shl(i64, i64, i32 signext, i32 signext)"),
    ("zeno_fit", "declare i64 @zeno_fit(i64, i64, i64, ptr, i32 signext, i32 signext)"),
    ("zeno_shr", "declare i64 @zeno_shr(i64, i64, i32 signext, i32 signext)"),
    ("zeno_range_count", "declare i64 @zeno_range_count(i64, i64, i64, i32 signext, i32 signext)"),
    ("zeno_range_list", "declare ptr @zeno_range_list(i64, i64, i64, i32 signext, i32 signext)"),
    ("zeno_floordiv_float", "declare double @zeno_floordiv_float(double, double, i32 signext, i32 signext)"),
    ("zeno_mod_float", "declare double @zeno_mod_float(double, double, i32 signext, i32 signext)"),
    ("zeno_int", "declare i64 @zeno_int(double, i32 signext, i32 signext)"),
    ("zeno_whole", "declare i64 @zeno_whole(double)"),
    ("zeno_sqrt", "declare double @zeno_sqrt(double, i32 signext, i32 signext)"),
    ("llvm.sqrt.f64", "declare double @llvm.sqrt.f64(double)"),
    ("pow", "declare double @pow(double, double)"),
    ("fabs", "declare double @fabs(double)"),
    ("floor", "declare double @floor(double)"),
    ("ceil", "declare double @ceil(double)"),
    ("sin", "declare double @sin(double)"),
    ("cos", "declare double @cos(double)"),
    ("tan", "declare double @tan(double)"),
    ("atan", "declare double @atan(double)"),
    ("atan2", "declare double @atan2(double, double)"),
    ("exp", "declare double @exp(double)"),
    ("round", "declare double @round(double)"),
    ("zeno_asin", "declare double @zeno_asin(double, i32 signext, i32 signext)"),
    ("zeno_acos", "declare double @zeno_acos(double, i32 signext, i32 signext)"),
    ("zeno_log", "declare double @zeno_log(double, i32 signext, i32 signext, i32 signext)"),
    ("strcmp", "declare i32 @strcmp(ptr, ptr)"),
    ("strstr", "declare ptr @strstr(ptr, ptr)"),
    ("zeno_alloc", "declare ptr @zeno_alloc(i64)"),
    ("zeno_alloc_text", "declare ptr @zeno_alloc_text(i64)"),
    ("zeno_join", "declare ptr @zeno_join(i32 signext, ...)"),
    ("zeno_print", "declare void @zeno_print(i32 signext, ...)"),
    ("zeno_len", "declare i64 @zeno_len(ptr)"),
    ("zeno_char_at", "declare ptr @zeno_char_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_str_int", "declare ptr @zeno_str_int(i64)"),
    ("zeno_str_float", "declare ptr @zeno_str_float(double)"),
    ("zeno_str_f32", "declare ptr @zeno_str_f32(float)"),
    ("zeno_f32", "declare float @zeno_f32(double, i32 signext, i32 signext)"),
    ("zeno_list_sum_f32", "declare float @zeno_list_sum_f32(ptr)"),
    ("zeno_str_bool", "declare ptr @zeno_str_bool(i1 zeroext)"),
    ("zeno_str_quantity", "declare ptr @zeno_str_quantity(double, ptr)"),
    ("zeno_repr_str", "declare ptr @zeno_repr_str(ptr)"),
    ("zeno_fixed", "declare ptr @zeno_fixed(double, i32 signext)"),
    ("zeno_fixed_quantity", "declare ptr @zeno_fixed_quantity(double, i32 signext, ptr)"),
    ("zeno_radix", "declare ptr @zeno_radix(i64, i8 signext)"),
    ("zeno_pad", "declare ptr @zeno_pad(ptr, i32 signext, i8 signext, i1 zeroext)"),
    ("zeno_parse_int", "declare i64 @zeno_parse_int(ptr, i32 signext, i32 signext)"),
    ("zeno_parse_float", "declare double @zeno_parse_float(ptr, i32 signext, i32 signext)"),
    ("zeno_upper", "declare ptr @zeno_upper(ptr)"),
    ("zeno_lower", "declare ptr @zeno_lower(ptr)"),
    ("zeno_strip", "declare ptr @zeno_strip(ptr)"),
    ("zeno_replace", "declare ptr @zeno_replace(ptr, ptr, ptr)"),
    ("zeno_starts_with", "declare zeroext i1 @zeno_starts_with(ptr, ptr)"),
    ("zeno_ends_with", "declare zeroext i1 @zeno_ends_with(ptr, ptr)"),
    ("zeno_split_blanks", "declare ptr @zeno_split_blanks(ptr)"),
    ("zeno_split", "declare ptr @zeno_split(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_lines", "declare ptr @zeno_lines(ptr)"),
    ("zeno_input", "declare ptr @zeno_input(ptr)"),
    ("zeno_program_start", "declare void @zeno_program_start(i32 signext, ptr)"),
    ("zeno_program_arguments", "declare ptr @zeno_program_arguments()"),
    ("zeno_program_exit", "declare void @zeno_program_exit(i64, i32 signext, i32 signext)"),
    ("zeno_program_environment", "declare ptr @zeno_program_environment(ptr)"),
    ("zeno_program_warn", "declare void @zeno_program_warn(ptr)"),
    ("zeno_chars", "declare ptr @zeno_chars(ptr)"),
    ("zeno_char_size", "declare i64 @zeno_char_size(ptr)"),
    ("zeno_char_here", "declare ptr @zeno_char_here(ptr, ptr)"),
    ("zeno_list_new", "declare ptr @zeno_list_new(i64)"),
    ("zeno_list_of", "declare ptr @zeno_list_of(i32 signext, ...)"),
    // A list of bytes (decision 36.2): the same struct, with a byte for
    // each item. A byte goes in and comes out as an i64.
    ("zeno_bytes_new", "declare ptr @zeno_bytes_new(i64)"),
    ("zeno_bytes_of", "declare ptr @zeno_bytes_of(i32 signext, ...)"),
    ("zeno_bytes_append", "declare void @zeno_bytes_append(ptr, i64)"),
    ("zeno_bytes_at", "declare ptr @zeno_bytes_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_bytes_copy", "declare ptr @zeno_bytes_copy(ptr)"),
    ("zeno_bytes_concat", "declare ptr @zeno_bytes_concat(ptr, ptr)"),
    ("zeno_bytes_repeat", "declare ptr @zeno_bytes_repeat(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_bytes_pop", "declare i64 @zeno_bytes_pop(ptr, i32 signext, i32 signext)"),
    ("zeno_bytes_pop_at", "declare i64 @zeno_bytes_pop_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_bytes_insert", "declare void @zeno_bytes_insert(ptr, i64, i64, i32 signext, i32 signext)"),
    ("zeno_bytes_str", "declare ptr @zeno_bytes_str(ptr)"),
    ("zeno_bytes_eq", "declare zeroext i1 @zeno_bytes_eq(ptr, ptr)"),
    ("zeno_bytes_eq_ints", "declare zeroext i1 @zeno_bytes_eq_ints(ptr, ptr)"),
    ("zeno_bytes_find", "declare i64 @zeno_bytes_find(ptr, i64)"),
    ("zeno_bytes_contains", "declare zeroext i1 @zeno_bytes_contains(ptr, i64)"),
    ("zeno_bytes_reverse", "declare void @zeno_bytes_reverse(ptr)"),
    ("zeno_bytes_clear", "declare void @zeno_bytes_clear(ptr)"),
    ("zeno_bytes_sort", "declare void @zeno_bytes_sort(ptr)"),
    ("zeno_bytes_sort_by", "declare void @zeno_bytes_sort_by(ptr, ptr, i32 signext, i32 signext, i32 signext)"),
    ("zeno_bytes_slice", "declare ptr @zeno_bytes_slice(ptr, i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    ("zeno_bytes_sum", "declare i64 @zeno_bytes_sum(ptr)"),
    ("zeno_bytes_best", "declare i64 @zeno_bytes_best(ptr, i1 zeroext, ptr, i32 signext, i32 signext)"),
    ("zeno_list_append", "declare void @zeno_list_append(ptr, i64)"),
    ("zeno_list_copy", "declare ptr @zeno_list_copy(ptr)"),
    ("zeno_list_drop", "declare void @zeno_list_drop(ptr)"),
    ("zeno_free", "declare void @zeno_free(ptr)"),
    ("zeno_error_drop", "declare void @zeno_error_drop(ptr)"),
    ("zeno_closure_drop", "declare void @zeno_closure_drop(ptr)"),
    ("zeno_closure_release", "declare void @zeno_closure_release(ptr)"),
    // Text that is kept, in a build with no collector (docs/text.md).
    ("zeno_text_keep", "declare ptr @zeno_text_keep(ptr)"),
    ("zeno_text_drop", "declare void @zeno_text_drop(ptr)"),
    ("zeno_text_lend", "declare ptr @zeno_text_lend(ptr)"),
    ("zeno_text_take", "declare ptr @zeno_text_take(ptr)"),
    ("zeno_text_return", "declare ptr @zeno_text_return(i64, ptr)"),
    ("zeno_list_drop_text", "declare void @zeno_list_drop_text(ptr)"),
    ("zeno_list_copy_text", "declare ptr @zeno_list_copy_text(ptr)"),
    ("zeno_list_keep_text", "declare ptr @zeno_list_keep_text(ptr)"),
    ("zeno_error_of", "declare ptr @zeno_error_of(ptr, ptr, ptr, ptr)"),
    ("zeno_list_concat", "declare ptr @zeno_list_concat(ptr, ptr)"),
    ("zeno_list_repeat", "declare ptr @zeno_list_repeat(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_list_concat_mixed", "declare ptr @zeno_list_concat_mixed(ptr, i1 zeroext, ptr, i1 zeroext)"),
    ("zeno_list_at", "declare ptr @zeno_list_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_list_pop", "declare i64 @zeno_list_pop(ptr, i32 signext, i32 signext)"),
    ("zeno_list_str", "declare ptr @zeno_list_str(ptr, ptr)"),
    ("zeno_list_eq", "declare zeroext i1 @zeno_list_eq(ptr, ptr, ptr)"),
    ("zeno_list_contains", "declare zeroext i1 @zeno_list_contains(ptr, i64, ptr)"),
    ("zeno_list_find", "declare i64 @zeno_list_find(ptr, i64, ptr)"),
    ("zeno_list_insert", "declare void @zeno_list_insert(ptr, i64, i64, i32 signext, i32 signext)"),
    ("zeno_list_pop_at", "declare i64 @zeno_list_pop_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_list_reverse", "declare void @zeno_list_reverse(ptr)"),
    ("zeno_list_clear", "declare void @zeno_list_clear(ptr)"),
    ("zeno_list_sort", "declare void @zeno_list_sort(ptr, i32 signext)"),
    ("zeno_list_sort_by", "declare void @zeno_list_sort_by(ptr, ptr, i32 signext, i32 signext, i32 signext)"),
    ("zeno_list_slice", "declare ptr @zeno_list_slice(ptr, i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    ("zeno_list_join", "declare ptr @zeno_list_join(ptr, ptr)"),
    // Dicts. A key or a value travels as a slot of 64 bits, as an item
    // of a list does.
    ("zeno_dict_new", "declare ptr @zeno_dict_new(i32 signext, i32 signext)"),
    ("zeno_dict_kinds", "declare ptr @zeno_dict_kinds(ptr, i32 signext, i32 signext)"),
    ("zeno_dict_set", "declare void @zeno_dict_set(ptr, i64, i64)"),
    ("zeno_dict_at", "declare i64 @zeno_dict_at(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_dict_find", "declare i64 @zeno_dict_find(ptr, i64)"),
    ("zeno_dict_value", "declare i64 @zeno_dict_value(ptr, i64)"),
    ("zeno_dict_get", "declare i64 @zeno_dict_get(ptr, i64, i64)"),
    ("zeno_dict_pop", "declare i64 @zeno_dict_pop(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_dict_keys", "declare ptr @zeno_dict_keys(ptr)"),
    ("zeno_dict_values", "declare ptr @zeno_dict_values(ptr)"),
    ("zeno_dict_clear", "declare void @zeno_dict_clear(ptr)"),
    ("zeno_dict_drop", "declare void @zeno_dict_drop(ptr)"),
    ("zeno_dict_str", "declare ptr @zeno_dict_str(ptr)"),
    ("zeno_dict_eq", "declare zeroext i1 @zeno_dict_eq(ptr, ptr)"),
    ("zeno_dict_str_with", "declare ptr @zeno_dict_str_with(ptr, ptr)"),
    ("zeno_dict_eq_with", "declare zeroext i1 @zeno_dict_eq_with(ptr, ptr, ptr)"),
    ("zeno_find", "declare i64 @zeno_find(ptr, ptr)"),
    ("zeno_slice", "declare ptr @zeno_slice(ptr, i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    // The `files` module. In a firmware build the four that can fail
    // are declared another way: see `DECLARED_FOR_FIRMWARE`.
    ("zeno_files_exists", "declare zeroext i1 @zeno_files_exists(ptr)"),
    ("zeno_files_read", "declare ptr @zeno_files_read(ptr, i32 signext, i32 signext)"),
    ("zeno_files_write", "declare void @zeno_files_write(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_append", "declare void @zeno_files_append(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_remove", "declare void @zeno_files_remove(ptr, i32 signext, i32 signext)"),
    ("zeno_files_read_bytes", "declare ptr @zeno_files_read_bytes(ptr, i32 signext, i32 signext)"),
    ("zeno_files_write_bytes", "declare void @zeno_files_write_bytes(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_list", "declare ptr @zeno_files_list(ptr, i32 signext, i32 signext)"),
    ("zeno_files_make_folder", "declare void @zeno_files_make_folder(ptr, i32 signext, i32 signext)"),
    // What a `files.Reader` and a `files.Writer` are made of (59.6).
    ("zeno_files_handle_open", "declare i64 @zeno_files_handle_open(ptr, i64, i32 signext, i32 signext)"),
    ("zeno_files_handle_line", "declare ptr @zeno_files_handle_line(i64, ptr, i32 signext, i32 signext)"),
    ("zeno_files_handle_bytes", "declare ptr @zeno_files_handle_bytes(i64, ptr, i64, i32 signext, i32 signext)"),
    ("zeno_files_handle_write", "declare void @zeno_files_handle_write(i64, ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_handle_write_bytes", "declare void @zeno_files_handle_write_bytes(i64, ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_handle_close", "declare void @zeno_files_handle_close(i64)"),
    ("zeno_char_code", "declare i64 @zeno_char_code(ptr, i32 signext, i32 signext)"),
    ("zeno_char", "declare ptr @zeno_char(i64, i32 signext, i32 signext)"),
    ("zeno_list_sum_int", "declare i64 @zeno_list_sum_int(ptr, i32 signext, i32 signext)"),
    ("zeno_list_sum_float", "declare double @zeno_list_sum_float(ptr)"),
    ("zeno_list_best_int", "declare i64 @zeno_list_best_int(ptr, i1 zeroext, ptr, i32 signext, i32 signext)"),
    ("zeno_list_best_float", "declare double @zeno_list_best_float(ptr, i1 zeroext, ptr, i32 signext, i32 signext)"),
    // A u64 (decision 42) travels as an i64 with the same bits.
    ("zeno_add_u64", "declare i64 @zeno_add_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_sub_u64", "declare i64 @zeno_sub_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_mul_u64", "declare i64 @zeno_mul_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_floordiv_u64", "declare i64 @zeno_floordiv_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_mod_u64", "declare i64 @zeno_mod_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_pow_u64", "declare i64 @zeno_pow_u64(i64, i64, i32 signext, i32 signext)"),
    ("zeno_shl_u64", "declare i64 @zeno_shl_u64(i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    ("zeno_shr_u64", "declare i64 @zeno_shr_u64(i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    ("zeno_shl_wrap_u64", "declare i64 @zeno_shl_wrap_u64(i64, i64, i1 zeroext, i32 signext, i32 signext)"),
    ("zeno_u64", "declare i64 @zeno_u64(i64, i32 signext, i32 signext)"),
    ("zeno_int_u64", "declare i64 @zeno_int_u64(i64, i32 signext, i32 signext)"),
    ("zeno_str_u64", "declare ptr @zeno_str_u64(i64)"),
    ("zeno_radix_u64", "declare ptr @zeno_radix_u64(i64, i8 signext)"),
    ("zeno_list_sum_u64", "declare i64 @zeno_list_sum_u64(ptr, i32 signext, i32 signext)"),
    ("zeno_list_best_u64", "declare i64 @zeno_list_best_u64(ptr, i1 zeroext, ptr, i32 signext, i32 signext)"),
    ("zeno_error_same", "declare zeroext i1 @zeno_error_same(ptr, ptr)"),
    ("zeno_error_new", "declare ptr @zeno_error_new(ptr, ptr, ptr)"),
    ("zeno_raise", "declare void @zeno_raise(ptr, i32 signext, i32 signext) noreturn"),
    // A firmware build's three: note an error, ask its kind, and stop.
    ("zeno_note", "declare void @zeno_note(ptr, i32 signext, i32 signext)"),
    ("zeno_is", "declare zeroext i1 @zeno_is(ptr, ptr)"),
    ("zeno_uncaught", "declare void @zeno_uncaught() noreturn"),
    ("zeno_raise_again", "declare void @zeno_raise_again() noreturn"),
    ("zeno_caught", "declare zeroext i1 @zeno_caught(ptr)"),
    ("zeno_try_size", "declare i64 @zeno_try_size()"),
    ("zeno_try_open", "declare void @zeno_try_open(ptr)"),
    ("zeno_try_close", "declare void @zeno_try_close(ptr)"),
    // `returns_twice` warns LLVM that this call can come back a second
    // time, much later, when an error jumps to it. What it takes is in
    // `SETJMP_TAKES`.
    ("_setjmp", if cfg!(windows) { "declare i32 @_setjmp(ptr, ptr) returns_twice" } else { "declare i32 @_setjmp(ptr) returns_twice" }),
    ("zeno_show_function", "declare ptr @zeno_show_function(ptr)"),
    ("zeno_mark_words", "declare void @zeno_mark_words(ptr, i64)"),
    // Only in a program built by `zeno test --build` (decision 60.2).
    ("zeno_test_out", "declare void @zeno_test_out(ptr)"),
    ("zeno_tests_begin", "declare void @zeno_tests_begin()"),
    ("zeno_test_begin", "declare void @zeno_test_begin(ptr)"),
    ("zeno_test_end", "declare void @zeno_test_end()"),
    ("zeno_tests_end", "declare i32 @zeno_tests_end()"),
    ("zeno_test_uncaught", "declare void @zeno_test_uncaught()"),
    ("zeno_test_false", "declare void @zeno_test_false(ptr, ptr, ptr, ptr, i32, i32)"),
    ("zeno_test_raised_nothing", "declare void @zeno_test_raised_nothing(ptr, i32, i32)"),
    ("zeno_test_raised_other", "declare void @zeno_test_raised_other(ptr, ptr, i32, i32)"),
];

/// A few functions of the runtime, written again in IR.
///
/// A call into `runtime.c` cannot be inlined, because the C is in
/// another file that LLVM does not see while it compiles this one. For
/// text that does not matter. For `%`, `//` and `/` it does: a loop of
/// them ran at a third of the speed of the C build. So these are
/// written here, in the same file as the program, where LLVM inlines
/// them. Each is copied into a program only if the program uses it.
///
/// They follow `zeno_mod`, `zeno_floordiv` and `zeno_div` in `runtime.c`
/// line for line. The dot in the names keeps them apart from anything C
/// can name.
///
/// The last six put a number that may be `none` into a list. The
/// runtime's own (`zeno_box_int`) take a C struct, and how a struct is
/// handed to a C function differs from processor to processor. Written
/// in IR, they never leave IR.
const WRITTEN: &[(&str, &str)] = &[
    (
        "zeno.floordiv",
        "; `//` on two ints. The processor rounds toward zero and Zeno rounds
; down, so step down once more when the signs differ and the division
; was not exact.
define internal i64 @zeno.floordiv(i64 %a, i64 %b, i32 %line, i32 %col) {
entry:
  %zero = icmp eq i64 %b, 0
  br i1 %zero, label %by_zero, label %check

by_zero:
  call void @zeno_by_zero(i32 %line, i32 %col)
  unreachable

check:
  ; The one division that does not fit: the smallest int by -1.
  %smallest = icmp eq i64 %a, -9223372036854775808
  %minus_one = icmp eq i64 %b, -1
  %too_big = and i1 %smallest, %minus_one
  br i1 %too_big, label %overflow, label %divide

overflow:
  call void @zeno_overflow(i32 %line, i32 %col)
  unreachable

divide:
  %quotient = sdiv i64 %a, %b
  %left_over = srem i64 %a, %b
  %inexact = icmp ne i64 %left_over, 0
  ; The sign bit of `a xor b` is set when the two signs differ.
  %mixed = xor i64 %a, %b
  %differ = icmp slt i64 %mixed, 0
  %step_down = and i1 %inexact, %differ
  %lower = sub i64 %quotient, 1
  %result = select i1 %step_down, i64 %lower, i64 %quotient
  ret i64 %result
}
",
    ),
    (
        "zeno.mod",
        "; `%` on two ints. The result takes the sign of the right side, to pair
; with `//`.
define internal i64 @zeno.mod(i64 %a, i64 %b, i32 %line, i32 %col) {
entry:
  %zero = icmp eq i64 %b, 0
  br i1 %zero, label %by_zero, label %divide

by_zero:
  call void @zeno_by_zero(i32 %line, i32 %col)
  unreachable

divide:
  ; Anything `% -1` is 0. Dividing by 1 instead gives that, and avoids
  ; the smallest int `% -1`, which the processor refuses.
  %minus_one = icmp eq i64 %b, -1
  %by = select i1 %minus_one, i64 1, i64 %b
  %left_over = srem i64 %a, %by
  %inexact = icmp ne i64 %left_over, 0
  %mixed = xor i64 %left_over, %b
  %differ = icmp slt i64 %mixed, 0
  %move = and i1 %inexact, %differ
  %moved = add i64 %left_over, %b
  %result = select i1 %move, i64 %moved, i64 %left_over
  ret i64 %result
}
",
    ),
    (
        "zeno.div",
        "; `/`, which reports a zero on the right.
define internal double @zeno.div(double %a, double %b, i32 %line, i32 %col) {
entry:
  %zero = fcmp oeq double %b, 0.0
  br i1 %zero, label %by_zero, label %divide

by_zero:
  call void @zeno_by_zero(i32 %line, i32 %col)
  unreachable

divide:
  %result = fdiv double %a, %b
  ret double %result
}
",
    ),
    (
        "zeno.sqrt",
        "; `math.sqrt`, which reports a number below zero. The runtime's
; `zeno_sqrt` says so; any other number is LLVM's own square root, which
; is one instruction where the runtime's would be a call.
define internal double @zeno.sqrt(double %x, i32 %line, i32 %col) {
entry:
  %below = fcmp olt double %x, 0.0
  br i1 %below, label %refuse, label %root

refuse:
  %never = call double @zeno_sqrt(double %x, i32 %line, i32 %col)
  ret double %never

root:
  %result = call double @llvm.sqrt.f64(double %x)
  ret double %result
}
",
    ),
    (
        "zeno.box.int",
        "; An `int or none` on its way into a list. A slot has no room for the
; flag, so the number goes into a small block of its own and the slot
; holds the address: null for `none`.
define internal ptr @zeno.box.int({ i1, i64 } %value) {
entry:
  %present = extractvalue { i1, i64 } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc_text(i64 8)
  %inside = extractvalue { i1, i64 } %value, 1
  store i64 %inside, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.unbox.int",
        "; And out of the list again.
define internal { i1, i64 } @zeno.unbox.int(ptr %box) {
entry:
  %none = icmp eq ptr %box, null
  br i1 %none, label %nothing, label %some

some:
  %inside = load i64, ptr %box
  %value = insertvalue { i1, i64 } { i1 true, i64 undef }, i64 %inside, 1
  ret { i1, i64 } %value

nothing:
  ret { i1, i64 } zeroinitializer
}
",
    ),
    (
        "zeno.box.float",
        "; A `float or none` on its way into a list (see `zeno.box.int`).
define internal ptr @zeno.box.float({ i1, double } %value) {
entry:
  %present = extractvalue { i1, double } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc_text(i64 8)
  %inside = extractvalue { i1, double } %value, 1
  store double %inside, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.unbox.float",
        "define internal { i1, double } @zeno.unbox.float(ptr %box) {
entry:
  %none = icmp eq ptr %box, null
  br i1 %none, label %nothing, label %some

some:
  %inside = load double, ptr %box
  %value = insertvalue { i1, double } { i1 true, double undef }, double %inside, 1
  ret { i1, double } %value

nothing:
  ret { i1, double } zeroinitializer
}
",
    ),
    (
        "zeno.box.bool",
        "; A `bool or none` on its way into a list (see `zeno.box.int`).
define internal ptr @zeno.box.bool({ i1, i1 } %value) {
entry:
  %present = extractvalue { i1, i1 } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc_text(i64 1)
  %inside = extractvalue { i1, i1 } %value, 1
  %byte = zext i1 %inside to i8
  store i8 %byte, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.unbox.bool",
        "define internal { i1, i1 } @zeno.unbox.bool(ptr %box) {
entry:
  %none = icmp eq ptr %box, null
  br i1 %none, label %nothing, label %some

some:
  %byte = load i8, ptr %box
  %inside = trunc i8 %byte to i1
  %value = insertvalue { i1, i1 } { i1 true, i1 undef }, i1 %inside, 1
  ret { i1, i1 } %value

nothing:
  ret { i1, i1 } zeroinitializer
}
",
    ),    (
        "zeno.box.keep.int",
        "; An `int or none` kept by a list with no collector (decision 56.14).
; As `zeno.box.int`, but the box is a block of the pool, which the list
; frees, and not of the text room.
define internal ptr @zeno.box.keep.int({ i1, i64 } %value) {
entry:
  %present = extractvalue { i1, i64 } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc(i64 8)
  %inside = extractvalue { i1, i64 } %value, 1
  store i64 %inside, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.box.take.int",
        "; An `int or none` leaving a list for good: the number, and the box
; is freed (decision 56.14).
define internal { i1, i64 } @zeno.box.take.int(ptr %box) {
entry:
  %value = call { i1, i64 } @zeno.unbox.int(ptr %box)
  call void @zeno_free(ptr %box)
  ret { i1, i64 } %value
}
",
    ),
    (
        "zeno.list.keep.boxes.int",
        "; A list just made whose boxes are still another list's (`+`, `*`, a
; slice, the copy a loop walks) gets a box of its own for each item
; (decision 56.14).
define internal ptr @zeno.list.keep.boxes.int(ptr %list) {
entry:
  %len = load i64, ptr %list
  %items.at = getelementptr inbounds { i64, i64, ptr }, ptr %list, i32 0, i32 2
  %items = load ptr, ptr %items.at
  br label %loop

loop:
  %i = phi i64 [ 0, %entry ], [ %next, %body ]
  %more = icmp slt i64 %i, %len
  br i1 %more, label %body, label %done

body:
  %slot = getelementptr inbounds i64, ptr %items, i64 %i
  %box = load ptr, ptr %slot
  %value = call { i1, i64 } @zeno.unbox.int(ptr %box)
  %kept = call ptr @zeno.box.keep.int({ i1, i64 } %value)
  store ptr %kept, ptr %slot
  %next = add i64 %i, 1
  br label %loop

done:
  ret ptr %list
}
",
    ),
    (
        "zeno.box.keep.float",
        "; A `float or none` kept by a list (see `zeno.box.keep.int`).
define internal ptr @zeno.box.keep.float({ i1, double } %value) {
entry:
  %present = extractvalue { i1, double } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc(i64 8)
  %inside = extractvalue { i1, double } %value, 1
  store double %inside, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.box.take.float",
        "; The same for a `float or none` (see `zeno.box.take.int`).
define internal { i1, double } @zeno.box.take.float(ptr %box) {
entry:
  %value = call { i1, double } @zeno.unbox.float(ptr %box)
  call void @zeno_free(ptr %box)
  ret { i1, double } %value
}
",
    ),
    (
        "zeno.list.keep.boxes.float",
        "; The same for a list of `float or none` (see `zeno.list.keep.boxes.int`).
define internal ptr @zeno.list.keep.boxes.float(ptr %list) {
entry:
  %len = load i64, ptr %list
  %items.at = getelementptr inbounds { i64, i64, ptr }, ptr %list, i32 0, i32 2
  %items = load ptr, ptr %items.at
  br label %loop

loop:
  %i = phi i64 [ 0, %entry ], [ %next, %body ]
  %more = icmp slt i64 %i, %len
  br i1 %more, label %body, label %done

body:
  %slot = getelementptr inbounds i64, ptr %items, i64 %i
  %box = load ptr, ptr %slot
  %value = call { i1, double } @zeno.unbox.float(ptr %box)
  %kept = call ptr @zeno.box.keep.float({ i1, double } %value)
  store ptr %kept, ptr %slot
  %next = add i64 %i, 1
  br label %loop

done:
  ret ptr %list
}
",
    ),
    (
        "zeno.box.keep.bool",
        "; A `bool or none` kept by a list (see `zeno.box.keep.int`).
define internal ptr @zeno.box.keep.bool({ i1, i1 } %value) {
entry:
  %present = extractvalue { i1, i1 } %value, 0
  br i1 %present, label %some, label %none

some:
  %box = call ptr @zeno_alloc(i64 1)
  %inside = extractvalue { i1, i1 } %value, 1
  %byte = zext i1 %inside to i8
  store i8 %byte, ptr %box
  ret ptr %box

none:
  ret ptr null
}
",
    ),
    (
        "zeno.box.take.bool",
        "; The same for a `bool or none` (see `zeno.box.take.int`).
define internal { i1, i1 } @zeno.box.take.bool(ptr %box) {
entry:
  %value = call { i1, i1 } @zeno.unbox.bool(ptr %box)
  call void @zeno_free(ptr %box)
  ret { i1, i1 } %value
}
",
    ),
    (
        "zeno.list.keep.boxes.bool",
        "; The same for a list of `bool or none` (see `zeno.list.keep.boxes.int`).
define internal ptr @zeno.list.keep.boxes.bool(ptr %list) {
entry:
  %len = load i64, ptr %list
  %items.at = getelementptr inbounds { i64, i64, ptr }, ptr %list, i32 0, i32 2
  %items = load ptr, ptr %items.at
  br label %loop

loop:
  %i = phi i64 [ 0, %entry ], [ %next, %body ]
  %more = icmp slt i64 %i, %len
  br i1 %more, label %body, label %done

body:
  %slot = getelementptr inbounds i64, ptr %items, i64 %i
  %box = load ptr, ptr %slot
  %value = call { i1, i1 } @zeno.unbox.bool(ptr %box)
  %kept = call ptr @zeno.box.keep.bool({ i1, i1 } %value)
  store ptr %kept, ptr %slot
  %next = add i64 %i, 1
  br label %loop

done:
  ret ptr %list
}
",
    ),
    (
        "zeno.list.drop.boxes",
        "; The end of a list of numbers that may be `none`, with no collector:
; each box, then the list (decision 56.14). Null means the list was
; given away.
define internal void @zeno.list.drop.boxes(ptr %list) {
entry:
  %gone = icmp eq ptr %list, null
  br i1 %gone, label %end, label %start

start:
  %len = load i64, ptr %list
  %items.at = getelementptr inbounds { i64, i64, ptr }, ptr %list, i32 0, i32 2
  %items = load ptr, ptr %items.at
  br label %loop

loop:
  %i = phi i64 [ 0, %start ], [ %next, %body ]
  %more = icmp slt i64 %i, %len
  br i1 %more, label %body, label %done

body:
  %slot = getelementptr inbounds i64, ptr %items, i64 %i
  %box = load ptr, ptr %slot
  call void @zeno_free(ptr %box)
  %next = add i64 %i, 1
  br label %loop

done:
  call void @zeno_list_drop(ptr %list)
  br label %end

end:
  ret void
}
",
    ),
];

/// The name of one of the functions above for the boxes of a list:
/// `box_function("box.keep", "int")` is `zeno.box.keep.int`.
fn box_function(what: &str, scalar: &str) -> &'static str {
    match (what, scalar) {
        ("unbox", "int") => "zeno.unbox.int",
        ("unbox", "float") => "zeno.unbox.float",
        ("unbox", _) => "zeno.unbox.bool",
        ("box.keep", "int") => "zeno.box.keep.int",
        ("box.keep", "float") => "zeno.box.keep.float",
        ("box.keep", _) => "zeno.box.keep.bool",
        ("box.take", "int") => "zeno.box.take.int",
        ("box.take", "float") => "zeno.box.take.float",
        ("box.take", _) => "zeno.box.take.bool",
        ("list.keep.boxes", "int") => "zeno.list.keep.boxes.int",
        ("list.keep.boxes", "float") => "zeno.list.keep.boxes.float",
        (_, _) => "zeno.list.keep.boxes.bool",
    }
}

// ------------------------------------------------------------------- Pieces

/// A value that has been worked out: how the IR refers to it, and what
/// kind of Zeno value it is. `code` is a name (`%t3`), a constant (`42`,
/// `true`, `1.5`) or the address of a piece of text (`@.str.2`).
#[derive(Clone)]
struct Val {
    code: String,
    kind: Kind,
    /// For text: it was there before this statement began (a literal, or
    /// what an unchanging variable holds), so it will still be there
    /// afterwards. Text that is not `fixed` is in the text room: this
    /// statement built it, or a function gave it back, or it was lent
    /// (see `lend`). Only a build with no collector cares: there, such
    /// text is gone when the statement is over, and is copied to be kept.
    fixed: bool,
    /// For text that was lent: where it was read from. See `keep`.
    lent: Option<String>,
    /// For a list: this very expression made it, and nobody has it yet.
    /// `[1, 2]` does, and a comprehension, `split`, `map`, `filter` and
    /// `+` on two lists.
    made: bool,
    /// The statement being written owns it and frees it when the
    /// statement is over. So it can be looked at, and it cannot be kept.
    /// See `belongs`.
    passing: bool,
}

/// A value that belongs to the statement being written. See
/// `Emitter::belongs`.
struct Passing {
    /// What it is held under, so that leaving early frees it. See `hold`.
    key: String,
    /// The slot that holds its address.
    slot: String,
    kind: Kind,
}

impl Val {
    fn new(code: impl Into<String>, kind: Kind) -> Val {
        Val { code: code.into(), kind, fixed: false, lent: None, made: false, passing: false }
    }

    /// A list that was made right here, and is nobody's yet.
    fn made(mut self) -> Val {
        self.made = true;
        self
    }

    fn fixed(mut self) -> Val {
        self.fixed = true;
        self
    }

    /// What a call that gives nothing back leaves behind.
    fn nothing() -> Val {
        Val::new("", Kind::None)
    }
}

/// Where a variable's value is.
#[derive(Clone)]
enum Place {
    /// It never changes, so the variable is simply another name for this
    /// value. Every `let` inside a function, and every parameter.
    Value(String),
    /// It lives in memory at this address, and is read with `load` and
    /// written with `store`. Every `var`, and everything declared at the
    /// top of the file, where functions must be able to find it.
    Slot(String),
}

#[derive(Clone)]
struct Variable {
    place: Place,
    kind: Kind,
    /// Other functions can reach it and change it: a `var` at the top of
    /// the file. What was known about such a variable before a call may
    /// not be true after it. See `present`.
    open: bool,
    /// It owns the list or object it holds, and frees it when its block
    /// ends. An owner that is a variable always has a slot, and the slot
    /// holds null once the value has been given away. See "Owned values".
    owner: bool,
    /// It holds a function with a name, written inside a function, that
    /// uses variables of the function around it (as in `emit_c`, decision
    /// 56.16): the declaration, and whether it may be used here.
    inner: Option<(Arc<Function>, bool)>,
}

/// One of the program's functions or methods, as a call needs to know it.
#[derive(Clone)]
struct Entry {
    decl: Arc<Function>,
    kind: FunctionKind,
    /// Its name in the IR, such as `@z_area`.
    name: String,
    /// For each parameter: does the function take over an owned value?
    owned: Vec<bool>,
    /// For each parameter: is it without `take`? Then the function only
    /// looks, and may be lent a value that is gone when the statement is.
    lends: Vec<bool>,
    /// Where its defaults are read: the named function it is written in
    /// (0 for the top of the file), and the functions with names written
    /// inside functions that it sees (as in `emit_c`, decision 56.16).
    within: usize,
    sight: Vec<(String, Entry)>,
}

/// What a call can be given: the name, kind and default of each place.
type Slots = Vec<(String, Kind, Option<Arc<Expr>>)>;

/// A class: an object is a block of memory holding its fields in order.
struct ClassEntry {
    fields: Slots,
    /// For each field, its size if it is a sized whole number such as
    /// `level: u8`. The struct holds it in that many bits (`i8`), and it
    /// is an `i64` everywhere else: see `load_field` and `store_field`.
    sized: Vec<Option<Width>>,
    methods: HashMap<String, Entry>,
    /// The name of its struct type in the IR, such as `%z_Point`.
    ty: String,
    /// For each field: does the object own what the field holds? Only an
    /// object of an `owned class` owns anything.
    owned_fields: Vec<bool>,
    /// For a class that is some interface: the number each of its
    /// objects carries in front of its fields, which says that it is an
    /// object of this class. See `pass_on`.
    number: Option<usize>,
    /// Is this an interface? Then it is the struct `{ i32 }`, the number
    /// alone, which every object that is seen as one begins with.
    interface: bool,
}

impl ClassEntry {
    /// Where field `index` is in the struct: one further on if the
    /// number of the class stands in front.
    fn at(&self, index: usize) -> usize {
        index + usize::from(self.number.is_some())
    }
}

/// An enum: each case with the names and kinds of the values it carries.
struct EnumEntry {
    cases: Vec<(String, Vec<(String, Kind)>)>,
    /// For each case, which of the values it carries have one owner,
    /// which is the enum value from then on.
    takes: Vec<Vec<bool>>,
}

/// A function that has been reached but not yet written out. `sees` is
/// the top of the file as it stood at the function's own line, because
/// those are the variables it can use (decision 3.6d).
struct Waiting {
    entry: Entry,
    span: Span,
    sees: HashMap<String, Variable>,
    /// For a function with a name written inside a function: the others
    /// of its kind that it sees (decision 56.16).
    sight: Vec<(String, Entry)>,
}

/// A loop that is being written: where `continue` and `break` jump to.
struct Loop {
    next: String,
    done: String,
    /// How many `try` blocks were open when the loop began. Leaving the
    /// loop from inside a newer one has to close it first.
    tries: usize,
    /// How many blocks were open when the loop's body began. `break` and
    /// `continue` end the ones opened since, and free what they own.
    scopes: usize,
}

/// A counting loop that has been opened: its counter, and its labels.
struct Count {
    counter: String,
    test: String,
    body: String,
    next: String,
    done: String,
    /// For a loop over a list: the list that is walked, and its kind. It
    /// is the loop's own, and is freed when the loop is over. See
    /// `close_loop`.
    walked: Option<(String, Kind)>,
    /// For a loop over the pairs of a dict: the list of its values, which
    /// is walked beside the list of its keys.
    values: Option<(String, Kind)>,
    /// For a loop that walks text, with no collector: the slot holding
    /// the last character that was not ASCII, freed when the loop is over.
    held: Option<String>,
}

/// The function being written.
///
/// A function in IR is a list of blocks. A block starts with a label and
/// must end with exactly one instruction that leaves it (`br`, `ret` or
/// `unreachable`), with none after it. `ended` keeps track of that: it is
/// true from the moment the block has been left until a new label opens
/// the next one.
struct Code {
    lines: Vec<String>,
    /// The `alloca`s, which all go at the top of the first block. One
    /// inside a loop would take new space on the stack on every trip.
    slots: Vec<String>,
    /// The label of the block being filled.
    block: String,
    ended: bool,
    temps: usize,
    labels: usize,
    /// How often each variable name has been given a slot, to tell two
    /// variables of one name apart.
    named: HashMap<String, usize>,
    loops: Vec<Loop>,
    /// What the function gives back. `None` for the top of the file.
    returns: Kind,
    in_function: bool,
    /// The `try` blocks that are open here, innermost last: the address
    /// of each one's `zeno_try`. In a firmware build there is no
    /// `zeno_try`, and this holds the label of each one's `except` blocks.
    tries: Vec<String>,
    /// For each of those, in a firmware build: the kinds of error that
    /// can arrive at its `except` blocks.
    arriving: Vec<BTreeSet<String>>,
    /// In a firmware build: this function can raise, so it gives back
    /// `true` or `false` and puts its answer where `%result` points.
    fallible: bool,
    /// For each `try` that is open: how many blocks were open when its
    /// body began. An error raised here ends the ones opened since.
    try_scopes: Vec<usize>,
    /// How many values have been held between two owners. See `hold`.
    holds: usize,
    /// In a build with no collector: the statement being written has
    /// built text, which it must give back when it is over.
    built: bool,
    /// How many times the top of the text room has been noted.
    marks: usize,
    /// The values that belong to the statement being written, which
    /// frees them when it is over. See `belongs`.
    passing: Vec<Passing>,
    /// How many such values there have been, to name their slots.
    owns: usize,
    /// The slots this function has on its own stack.
    on_stack: HashSet<String>,
    /// There is a `try` somewhere in this function. See `load`.
    careful: bool,
    /// The names that functions written inside this one mention. A `var`
    /// with one of these names is shared with such a function, so it
    /// cannot live on this function's stack. See `declare`.
    shared: HashSet<String>,
    /// The resources whose blocks are open here, first made first
    /// (decision 59.1): the block each one's variable is in, and its
    /// name. Each way out of a block releases them, last made first,
    /// before it frees (`free_owned`).
    resources: Vec<(usize, String)>,
}

impl Code {
    fn new(returns: Kind, in_function: bool) -> Code {
        Code {
            lines: Vec::new(),
            slots: Vec::new(),
            block: "entry".to_string(),
            ended: false,
            temps: 0,
            labels: 0,
            named: HashMap::new(),
            loops: Vec::new(),
            returns,
            in_function,
            tries: Vec::new(),
            arriving: Vec::new(),
            fallible: false,
            try_scopes: Vec::new(),
            holds: 0,
            built: false,
            marks: 0,
            passing: Vec::new(),
            owns: 0,
            on_stack: HashSet::new(),
            careful: false,
            shared: HashSet::new(),
            resources: Vec::new(),
        }
    }

    /// The same, for a function with these statements in it.
    /// `jumps` says whether errors jump (an ordinary build) or travel as
    /// values (a firmware build). Only a jump needs the care.
    fn of(body: &[Stmt], returns: Kind, in_function: bool, jumps: bool) -> Code {
        let mut code = Code::new(returns, in_function);
        code.careful = jumps && emit_c::has_try(body);
        for statement in body {
            emit_c::names_in_functions(statement, false, &mut code.shared);
        }
        code
    }
}

struct Emitter<'a> {
    types: &'a Types,
    /// `--release`: whole-number `+`, `-` and `*` wrap round, unchecked.
    release: bool,
    /// `--fast`: no index check on a list, and `/` and `math.sqrt` of
    /// floats are LLVM's own instructions (decision 23.1).
    fast: bool,
    /// The `for` loop being opened has a body that cannot change its
    /// list, so it needs no copy of it (`ast::walks_in_place`).
    in_place: bool,
    /// For a `for` loop over a variable holding text: the body never
    /// names the variable, so the text stays put while the loop walks it.
    text_stays: bool,
    /// The fields this function reads once, in its entry block: the
    /// parameters, the fields written somewhere in it, and the loads made
    /// so far (decision 22.8, `ast::fields_read_once`).
    once: Option<(HashSet<String>, HashSet<String>, HashMap<(String, String), Val>)>,
    /// Which values have one owner, and so what freeing a list frees.
    /// The C backend's, so that both agree.
    drops: emit_c::Drops,
    /// `zeno build --llvm --firmware`: errors travel as values that each
    /// caller tests, and nothing jumps. See "Errors as values".
    firmware: bool,
    /// There is a garbage collector in the program. `zeno build --llvm
    /// --no-collector` leaves it out: see "With no collector to help".
    collector: bool,
    /// `zeno build --memory`: report the memory used as the program ends.
    memory: bool,
    /// The call about to be made gives a list[u8], so a `map` makes
    /// a list of bytes (decision 36.2).
    makes_bytes: bool,
    /// What a `for` loop is about to walk. If that is a list made right
    /// there, as in `for pin in [2, 3, 5]`, the loop frees it.
    walked: Option<usize>,
    functions: HashMap<String, Entry>,
    classes: HashMap<String, ClassEntry>,
    /// For each interface, the classes that are one, each with its number.
    is_a: HashMap<String, Vec<(usize, String)>>,
    /// How many classes have been given a number.
    numbered: usize,
    enums: HashMap<String, EnumEntry>,
    /// Each enum case by name: the enum it belongs to, and its number.
    cases: HashMap<String, (String, usize)>,
    /// The copies of generic enums, `Maybe[int]`, whose cases are not in
    /// `cases`: each copy has a case of the same name (decision 58.1).
    copied_enums: HashSet<String>,
    /// The names brought in by `import`, and the module of each.
    imported: HashMap<String, String>,
    waiting: VecDeque<Waiting>,
    /// Functions with names written inside functions (decision 56.16), as
    /// in `emit_c`: how many there have been, for their names; for each
    /// block, the ones it declares with what each name meant before; the
    /// ones in sight of such a function being written now; the function
    /// whose arguments are being bound, with the object of a method; and
    /// the functions of the top of the file, as a default reads them.
    inner_count: usize,
    inner_frames: Vec<Vec<(String, Option<Entry>)>>,
    inner_seen: Vec<(String, Entry)>,
    callee: Option<(usize, Vec<(String, Entry)>, Option<Val>)>,
    /// Where the later of several values go, for the call being written
    /// (decision 57): slots in a `let a, b = f()`, or this function's own
    /// pointers in a `return f()`.
    outs: Option<Vec<String>>,
    top_functions: HashMap<String, Entry>,
    /// The variables in view, innermost block last. The first entry is
    /// the top of the file.
    scopes: Vec<HashMap<String, Variable>>,
    code: Code,
    /// How many functions written as values there have been, to give
    /// each a name.
    closures: usize,
    /// The declaration of the named function being compiled, 0 at the
    /// top of the file: with a line and a column, where the checker's
    /// notes are kept (`checker::Spot`).
    within: usize,

    // The finished parts of the file.
    /// The struct types: one for each class and each enum case.
    structs: Vec<String>,
    /// Each piece of text the program mentions, and its name in the IR.
    strings: Vec<(String, String)>,
    globals: Vec<String>,
    /// The globals that may hold an address, which the collector must be
    /// shown.
    roots: Vec<String>,
    /// The functions the compiler writes for itself: to make an object,
    /// show one, compare two. `written` has their names, so that each is
    /// written once.
    helpers: Vec<String>,
    written: HashSet<String>,
    definitions: Vec<String>,
    /// The top of the file, as the function `zeno_program`.
    program: String,
    /// The names from `DECLARED` and `WRITTEN` that are called.
    used: BTreeSet<&'static str>,
    /// Built by `zeno test --build`: the tests to run after the program,
    /// and an `assert` is compiled (decision 60.2).
    tests: Option<TestBuild>,
    /// The tests have been run already: at the end of the innermost
    /// resource at the top of the file, before it is released (rulings
    /// of decision 59.1).
    tests_run: bool,
}

/// The runtime functions that are called differently in a firmware
/// build (`zeno build --llvm --firmware`). There they say whether they
/// worked, and give their answer through one more parameter.
const DECLARED_FOR_FIRMWARE: &[(&str, &str)] = &[
    ("zeno_parse_int", "declare zeroext i1 @zeno_parse_int(ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_parse_float", "declare zeroext i1 @zeno_parse_float(ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_files_read", "declare zeroext i1 @zeno_files_read(ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_files_write", "declare zeroext i1 @zeno_files_write(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_append", "declare zeroext i1 @zeno_files_append(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_remove", "declare zeroext i1 @zeno_files_remove(ptr, i32 signext, i32 signext)"),
    ("zeno_files_read_bytes", "declare zeroext i1 @zeno_files_read_bytes(ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_files_write_bytes", "declare zeroext i1 @zeno_files_write_bytes(ptr, ptr, i32 signext, i32 signext)"),
    ("zeno_files_list", "declare zeroext i1 @zeno_files_list(ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_files_make_folder", "declare zeroext i1 @zeno_files_make_folder(ptr, i32 signext, i32 signext)"),
    ("zeno_files_handle_open", "declare zeroext i1 @zeno_files_handle_open(ptr, i64, i32 signext, i32 signext, ptr)"),
    ("zeno_files_handle_line", "declare zeroext i1 @zeno_files_handle_line(i64, ptr, i32 signext, i32 signext, ptr)"),
    ("zeno_files_handle_bytes", "declare zeroext i1 @zeno_files_handle_bytes(i64, ptr, i64, i32 signext, i32 signext, ptr)"),
    ("zeno_files_handle_write", "declare zeroext i1 @zeno_files_handle_write(i64, ptr, ptr, i32 signext, i32 signext)"),
    (
        "zeno_files_handle_write_bytes",
        "declare zeroext i1 @zeno_files_handle_write_bytes(i64, ptr, ptr, i32 signext, i32 signext)",
    ),
];

/// The error for a list that would outlive the statement that owns it:
/// one that is put somewhere that is not an owner.
fn no_owner(span: Span) -> Error {
    emit_c::without_collector("this list has no owner. Give it one, as in `let items: owned list[int] = [1, 2, 3]`", span)
}

/// The error for something a firmware build has to refuse. The words
/// are the C backend's, so that one message means one thing.
fn firmware_cannot(what: impl AsRef<str>, span: Span) -> Error {
    Error::new(format!("{}, or build without `--firmware`", what.as_ref()), span)
}

/// The error for a part of Zeno this stage does not handle yet.
fn cannot(what: impl AsRef<str>, span: Span) -> Error {
    Error::new(
        format!("`zeno build --llvm` cannot compile {} yet. `zeno run` can still run this program", what.as_ref()),
        span,
    )
}

/// The type written as the first argument of `allocate` or `register`,
/// and the argument after it.
fn written_type<'a>(what: &str, args: &'a [Arg], span: Span) -> Result<(Pointee, &'a Expr), Error> {
    match args {
        [Arg { name: None, value: Expr { kind: ExprKind::Name(item), .. } }, Arg { name: None, value }] => {
            match Pointee::named(item) {
                Some(pointee) => Ok((pointee, value)),
                None => Err(cannot(format!("`{what}` of `{item}`"), span)),
            }
        }
        _ => Err(cannot(format!("`{what}` called this way"), span)),
    }
}

/// The pin the LED is on. The same number as `ZENO_LED` in the runtime
/// and `board::LED` in the interpreter.
const LED: i64 = crate::board::LED;

/// A kind in words, for a message.
fn described(kind: &Kind) -> String {
    match kind {
        Kind::Int => "an int".to_string(),
        Kind::Byte => "a byte".to_string(),
        Kind::Float => "a float".to_string(),
        Kind::F32 => "an f32".to_string(),
        Kind::U64 => "a u64".to_string(),
        Kind::Str => "a str".to_string(),
        Kind::Bool => "a bool".to_string(),
        Kind::Pin => "a pin".to_string(),
        Kind::InputPin => "an input pin".to_string(),
        Kind::Serial => "a serial port".to_string(),
        Kind::AnalogPin => "an analog pin".to_string(),
        Kind::PwmPin => "a PWM pin".to_string(),
        Kind::Ticker => "a ticker".to_string(),
        Kind::Interrupt => "an interrupt".to_string(),
        Kind::Pointer(_) => "a pointer".to_string(),
        Kind::Register(_) => "a register".to_string(),
        Kind::None => "a value that is nothing".to_string(),
        Kind::Quantity(unit) => format!("a number with a unit ({unit})"),
        Kind::List(_) => "a list".to_string(),
        Kind::Dict(..) => "a dict".to_string(),
        Kind::Class(name) => format!("an object of the class `{name}`"),
        Kind::Enum(name) => format!("a value of the enum `{name}`"),
        Kind::Optional(_) => "a value that may be `none`".to_string(),
        Kind::Error => "an error value".to_string(),
        Kind::Function(_) => "a function used as a value".to_string(),
        Kind::Owned(_) => "an owned value".to_string(),
        Kind::Other(name) => format!("a value of type {name}"),
        Kind::CPointer(to) => format!("a C pointer to {to}"),
        Kind::CLibrary(_) => "a library of C".to_string(),
        Kind::Several(kinds) => format!("{} values", kinds.len()),
    }
}

/// Is this a kind that is held through an address?
fn is_pointer(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Str
            | Kind::List(_)
            | Kind::Dict(..)
            | Kind::Class(_)
            | Kind::Enum(_)
            | Kind::Error
            | Kind::Function(_)
            | Kind::Pointer(_)
    )
}

/// Can a value of this kind be, or contain, the address of memory that
/// the collector looks after? Numbers and bools cannot.
fn holds_pointer(kind: &Kind) -> bool {
    match kind {
        Kind::Int | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Quantity(_) | Kind::None => false,
        Kind::Optional(inner) => holds_pointer(inner),
        _ => true,
    }
}

/// A kind without the word `owned` on it. An owned value is held exactly
/// as a shared one is. What differs is who frees it, and that is kept
/// beside the kind: see `Variable::owner`.
fn shared(kind: &Kind) -> Kind {
    emit_c::disowned(kind.clone())
}

/// The kind of `[]` before the place it arrives at has said what it holds.
fn is_empty_list(kind: &Kind) -> bool {
    matches!(kind, Kind::List(item) if matches!(**item, Kind::Other(_)))
}

/// The kind of `[:]` before the place it arrives at has said what it holds.
fn is_empty_dict(kind: &Kind) -> bool {
    matches!(kind, Kind::Dict(key, _) if matches!(**key, Kind::Other(_)))
}

/// What a dict is told about its keys and its values when it is made, as
/// the two numbers the runtime takes: the letters 's', 'i', 'f' and 'b'
/// (see `emit_c::dict_kinds`).
fn dict_kinds(key: &Kind, value: &Kind, span: Span) -> Result<String, Error> {
    let (key, value) = emit_c::dict_kinds(key, value, span)?;
    Ok(format!("i32 {}, i32 {}", key as u32, value as u32))
}

/// The one kind that values of kinds `a` and `b` can both be stored as, if
/// there is one: `int` and `none` meet in `int or none`.
fn joined(a: &Kind, b: &Kind) -> Option<Kind> {
    match (a, b) {
        _ if a == b => Some(a.clone()),
        // `[[], [1]]`: the empty list goes along with the other one.
        (empty, Kind::List(_)) if is_empty_list(empty) => Some(b.clone()),
        (Kind::List(_), empty) if is_empty_list(empty) => Some(a.clone()),
        // `[:]` beside a dict that says what it holds.
        (empty, Kind::Dict(..)) if is_empty_dict(empty) => Some(b.clone()),
        (Kind::Dict(..), empty) if is_empty_dict(empty) => Some(a.clone()),
        (Kind::None, Kind::Optional(_)) => Some(b.clone()),
        (Kind::Optional(_), Kind::None) => Some(a.clone()),
        (Kind::None, other) | (other, Kind::None) => Some(Kind::Optional(Box::new(other.clone()))),
        (Kind::Optional(inner), other) | (other, Kind::Optional(inner)) if **inner == *other => {
            Some(Kind::Optional(inner.clone()))
        }
        // Two functions that take and give back the same things.
        (Kind::Function(x), Kind::Function(y)) => (signature(x).ok()? == signature(y).ok()?).then(|| a.clone()),
        _ => None,
    }
}

/// A short name for a kind, to build the names of helper functions from.
fn key(kind: &Kind) -> String {
    match kind {
        Kind::Int => "int".to_string(),
        Kind::Byte => "u8".to_string(),
        Kind::Float => "float".to_string(),
        Kind::F32 => "f32".to_string(),
        Kind::U64 => "u64".to_string(),
        Kind::Bool => "bool".to_string(),
        Kind::Pin => "pin".to_string(),
        Kind::InputPin => "input_pin".to_string(),
        Kind::Serial => "serial".to_string(),
        Kind::AnalogPin => "analog_pin".to_string(),
        Kind::PwmPin => "pwm_pin".to_string(),
        Kind::Ticker => "ticker".to_string(),
        Kind::Interrupt => "interrupt".to_string(),
        Kind::Pointer(pointee) => format!("pointer_{}", pointee.name()),
        Kind::Register(width) => format!("register_{}", width.name),
        Kind::Str => "str".to_string(),
        Kind::None => "none".to_string(),
        Kind::Quantity(unit) => format!("in_{unit}"),
        Kind::List(item) => format!("list_{}", key(item)),
        Kind::Dict(keys, values) => format!("dict_{}_{}", key(keys), key(values)),
        Kind::Class(name) | Kind::Enum(name) => name.clone(),
        Kind::Optional(inner) => format!("opt_{}", key(inner)),
        Kind::Error => "error".to_string(),
        Kind::Function(_) => "function".to_string(),
        Kind::Other(_) => "other".to_string(),
        Kind::CPointer(_) => "c_pointer".to_string(),
        Kind::CLibrary(number) => format!("c_library_{number}"),
        Kind::Owned(inner) => key(inner),
        Kind::Several(kinds) => format!("several_{}", kinds.iter().map(key).collect::<Vec<_>>().join("_")),
    }
}

/// The runtime function that does `what` to a list of `item`s:
/// `zeno_list_append`, or `zeno_bytes_append` for a list of bytes (36.2).
fn list_fn(item: &Kind, what: &'static str) -> &'static str {
    let bytes = *item == Kind::Byte;
    match (what, bytes) {
        ("new", true) => "zeno_bytes_new",
        ("new", false) => "zeno_list_new",
        ("append", true) => "zeno_bytes_append",
        ("append", false) => "zeno_list_append",
        ("copy", true) => "zeno_bytes_copy",
        ("copy", false) => "zeno_list_copy",
        ("concat", true) => "zeno_bytes_concat",
        ("concat", false) => "zeno_list_concat",
        ("pop", true) => "zeno_bytes_pop",
        ("pop", false) => "zeno_list_pop",
        ("pop_at", true) => "zeno_bytes_pop_at",
        ("pop_at", false) => "zeno_list_pop_at",
        ("insert", true) => "zeno_bytes_insert",
        ("insert", false) => "zeno_list_insert",
        ("reverse", true) => "zeno_bytes_reverse",
        ("reverse", false) => "zeno_list_reverse",
        ("clear", true) => "zeno_bytes_clear",
        ("clear", false) => "zeno_list_clear",
        ("slice", true) => "zeno_bytes_slice",
        ("slice", false) => "zeno_list_slice",
        ("sort_by", true) => "zeno_bytes_sort_by",
        ("sort_by", false) => "zeno_list_sort_by",
        _ => unreachable!("no runtime function for `{what}` on a list"),
    }
}

/// The kind of an item once it is read out of a list: a byte is an int.
fn read_kind(item: &Kind) -> Kind {
    match item {
        Kind::Byte => Kind::Int,
        item => item.clone(),
    }
}

/// The IR type that holds a Zeno value of this kind.
fn ir_type(kind: &Kind, span: Span) -> Result<&'static str, Error> {
    Ok(match kind {
        // A pin of the board is its number, and a register its address.
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "i64",
        // A u64 is the same 64 bits; the operations on it are the
        // unsigned ones (decision 42.8).
        Kind::U64 => "i64",
        // Only ever an item of a list of bytes (36.2), and it travels
        // as a whole number, as an int does.
        Kind::Byte => "i64",
        // A raw pointer is an address, and nothing more.
        Kind::Pointer(_) => "ptr",
        // A quantity is a float. Its unit has done its work in the
        // checker and costs nothing in the compiled program.
        // An f32 travels as the double it is exactly, and every sum on
        // one is rounded to 32 bits (decision 41.7).
        Kind::Float | Kind::F32 | Kind::Quantity(_) => "double",
        // One bit.
        Kind::Bool => "i1",
        // Text is the address of its first byte, as in the C backend.
        // Lists, objects, enum values, errors and functions live in
        // memory the collector looks after, and are held by address
        // too, because in Zeno they are shared: `let b = a` makes a
        // second name for the same object, not a copy.
        Kind::Str | Kind::List(_) | Kind::Dict(..) | Kind::Class(_) | Kind::Enum(_) | Kind::Error | Kind::Function(_) => {
            "ptr"
        }
        // An address can already say "nothing": it is null. A number or
        // a bool cannot, so it travels as a pair with a flag in front.
        Kind::Optional(inner) => match &**inner {
            inner if is_pointer(inner) => "ptr",
            Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => "{ i1, i64 }",
            Kind::Float | Kind::F32 | Kind::Quantity(_) => "{ i1, double }",
            Kind::Bool => "{ i1, i1 }",
            _ => return Err(cannot(emit_c::optional_named(inner), span)),
        },
        Kind::Owned(inner) => ir_type(inner, span)?,
        // A function that gives several values gives the first as its
        // answer, and the rest through pointers (decision 57).
        Kind::Several(kinds) => ir_type(&kinds[0], span)?,
        Kind::None => {
            return Err(emit_c::only_none("`zeno build --llvm`", span))
        }
        // `import c` is built through C, not LLVM, so far (decision 44.10).
        Kind::Other(_) | Kind::CPointer(_) | Kind::CLibrary(_) => return Err(cannot(described(kind), span)),
    })
}

/// The pointers the later of several values go out through, after the
/// parameters (decision 57): `ptr %gives.2` for the second value.
fn gives_params(returns: &Kind) -> Vec<String> {
    (0..emit_c::later_values(returns).len()).map(|at| format!("ptr %gives.{}", at + 2)).collect()
}

/// As `ir_type`, for what a function gives back, which may be nothing.
fn return_type(kind: &Kind, span: Span) -> Result<&'static str, Error> {
    match kind {
        Kind::None => Ok("void"),
        _ => ir_type(kind, span),
    }
}

/// The IR type of the code of a function, for telling whether two
/// function kinds are called the same way.
fn signature(kind: &FunctionKind) -> Result<String, Error> {
    let nowhere = Span { line: 0, col: 0 };
    let mut params = vec!["ptr"];
    for param in &kind.params {
        params.push(ir_type(param, nowhere)?);
    }
    Ok(format!("{} ({})", return_type(&kind.returns, nowhere)?, params.join(", ")))
}

/// What a value of an IR type is before it has been given: all zeros.
fn zero(ty: &str) -> &'static str {
    match ty {
        "i64" => "0",
        "double" => "0.0",
        "i1" => "false",
        "ptr" => "null",
        _ => "zeroinitializer",
    }
}

/// Which of the three flagged kinds holds an optional `inner`.
fn scalar(inner: &Kind, span: Span) -> Result<&'static str, Error> {
    match inner {
        Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => Ok("int"),
        Kind::Float | Kind::F32 | Kind::Quantity(_) => Ok("float"),
        Kind::Bool => Ok("bool"),
        _ => Err(cannot(emit_c::optional_named(inner), span)),
    }
}

/// `line, col` as the last two arguments of a runtime call that can fail.
fn at(span: Span) -> String {
    format!("i32 {}, i32 {}", span.line, span.col)
}

/// The functions to compile for a function written in the program: the
/// function itself, or, if it is generic, each of its copies.
fn copies_of_function<'a>(types: &'a Types, decl: &'a Arc<Function>) -> Vec<&'a Arc<Function>> {
    match decl.type_params.is_empty() {
        true => vec![decl],
        false => types.function_copies(decl).iter().collect(),
    }
}

/// The same for a class.
fn copies_of_class<'a>(types: &'a Types, decl: &'a Class) -> Vec<&'a Class> {
    match decl.type_params.is_empty() {
        true => vec![decl],
        false => types.class_copies(decl).iter().map(|copy| &**copy).collect(),
    }
}

/// A name in the IR: `sigil` is `%` for something inside a function and
/// `@` for something global. Names with anything but plain letters and
/// digits in them are written in quotes, which IR allows.
fn named(sigil: char, prefix: &str, name: &str) -> String {
    if name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.') {
        return format!("{sigil}{prefix}{name}");
    }
    let mut quoted = String::new();
    for byte in name.bytes() {
        match byte {
            b'"' | b'\\' | 0..=0x1f | 0x7f.. => quoted += &format!("\\{byte:02X}"),
            _ => quoted.push(byte as char),
        }
    }
    format!("{sigil}\"{prefix}{quoted}\"")
}

/// A float as IR writes one. IR wants a decimal point in it, and has no
/// spelling for infinity, so anything unusual is written as the 64 bits
/// that make up the float.
fn float_code(x: f64) -> String {
    if !x.is_finite() {
        return format!("0x{:016X}", x.to_bits());
    }
    let text = format!("{x:?}");
    match text.split_once('e') {
        Some((digits, power)) if !digits.contains('.') => format!("{digits}.0e{power}"),
        _ => text,
    }
}

/// The size in bytes of an IR type, as IR can say it without knowing the
/// processor: the address of the second item in an array that starts at
/// address zero.
fn size_of(ty: &str) -> String {
    format!("ptrtoint (ptr getelementptr ({ty}, ptr null, i32 1) to i64)")
}

/// What "nothing" is for a value that may be `none`.
fn nothing(inner: &Kind, span: Span) -> Result<&'static str, Error> {
    if is_pointer(inner) {
        return Ok("null");
    }
    // A flag that says "not here", and whatever is beside it.
    scalar(inner, span).map(|_| "zeroinitializer")
}

/// The struct type of an enum case that carries values, such as
/// `%z_Shape.Circle`: its number first, then what it carries.
fn case_type(owner: &str, case: &str) -> String {
    named('%', "z_", &format!("{owner}.{case}"))
}

/// The one fixed value that stands for an enum case that carries nothing.
fn bare_case(owner: &str, case: &str) -> String {
    named('@', "zeno.case.", &format!("{owner}.{case}"))
}

/// How the IR sees the runtime's `zeno_error`: its kind, its message, and
/// the object that was raised, if one was.
const ERROR: &str = "{ ptr, ptr, ptr }";

// ----------------------------------------------------------------- The file

/// Turn a checked program into the text of one LLVM IR file. `file` is
/// the name of the Zeno source, which the compiled program uses in its
/// error messages. `build` says which of the three kinds of build this
/// is, as for the C backend. `memory` is `zeno build --memory`: the
/// program reports how much memory it used as it ends.
#[cfg(test)]
pub fn emit(program: &Program, types: &Types, file: &str, build: Build, memory: bool) -> Result<String, Error> {
    emit_with(program, types, file, build, memory, emit_c::Options::default())
}

/// `emit_with`, for `zeno test --build`: the program, with each of its
/// tests run after it and reported as `zeno test` reports it (decision
/// 60.2). The tests are functions of the program by now (see
/// `test_build::with_tests`).
pub fn emit_tests(
    program: &Program,
    types: &Types,
    file: &str,
    build: Build,
    memory: bool,
    options: emit_c::Options,
    tests: &TestBuild,
) -> Result<String, Error> {
    emit_all(program, types, file, build, memory, options, Some(tests))
}

/// The part of the runtime that a test build adds, as the file beside
/// the IR has it: with its functions visible to the IR (see `beside_ir`).
pub fn tests_runtime(tests: &TestBuild, build: Build) -> String {
    let text = tests.runtime(build != Build::NoCollector);
    text.lines().map(|line| line.strip_prefix("static ").unwrap_or(line).to_string() + "\n").collect()
}

/// `emit`, with the options of `emit_c::Options`.
pub fn emit_with(
    program: &Program,
    types: &Types,
    file: &str,
    build: Build,
    memory: bool,
    options: emit_c::Options,
) -> Result<String, Error> {
    emit_all(program, types, file, build, memory, options, None)
}

fn emit_all(
    program: &Program,
    types: &Types,
    file: &str,
    build: Build,
    memory: bool,
    options: emit_c::Options,
    tests: Option<&TestBuild>,
) -> Result<String, Error> {
    let (firmware, collector) = (build != Build::Ordinary, build != Build::NoCollector);
    let mut emitter = Emitter {
        release: options.release || options.fast,
        fast: options.fast,
        in_place: false,
        text_stays: false,
        once: None,
        types,
        drops: emit_c::Drops::of(program, types, collector),
        firmware,
        collector,
        memory,
        makes_bytes: false,
        walked: None,
        functions: HashMap::new(),
        classes: HashMap::new(),
        is_a: HashMap::new(),
        numbered: 0,
        enums: HashMap::new(),
        cases: HashMap::new(),
        copied_enums: HashSet::new(),
        imported: HashMap::new(),
        waiting: VecDeque::new(),
        inner_count: 0,
        inner_frames: Vec::new(),
        inner_seen: Vec::new(),
        callee: None,
        outs: None,
        top_functions: HashMap::new(),
        scopes: vec![HashMap::new()],
        code: Code::of(&program.statements, Kind::None, false, !firmware),
        closures: 0,
        within: 0,
        structs: Vec::new(),
        strings: Vec::new(),
        globals: Vec::new(),
        roots: Vec::new(),
        helpers: Vec::new(),
        written: HashSet::new(),
        definitions: Vec::new(),
        program: String::new(),
        used: BTreeSet::new(),
        tests: tests.cloned(),
        tests_run: false,
    };

    // A function can be called above the line it is written on, and a
    // class used above its own, so all of them are made known before
    // anything is compiled. What follows the `let` of a resource at the
    // top of the file is more of the top of the file (decision 59.1).
    for statement in crate::ast::flat(&program.statements) {
        match &statement.kind {
            // A generic function is not compiled. Its copies are, one
            // for each set of types the program calls it with. The
            // checker made them, and each is an ordinary function with a
            // name of its own: `@"z_first[int]"`, `@"z_first[str]"`.
            StmtKind::Function(decl) => {
                for decl in copies_of_function(types, decl) {
                    // `z_` keeps the program's names apart from the
                    // runtime's and from the C library's: a Zeno function
                    // may well be called `main`.
                    let entry = emitter.entry(decl, named('@', "z_", &decl.name), statement.span)?;
                    emitter.functions.insert(decl.name.clone(), entry);
                }
            }
            // The same for a generic class: `%"z_Stack[int]"`.
            StmtKind::Class(decl) => {
                for decl in copies_of_class(types, decl) {
                    emitter.register_class(decl, statement.span)?;
                }
            }
            // The same for a generic enum: `Maybe[int]` (decision 58.1).
            StmtKind::Enum(decl) if !decl.type_params.is_empty() => {
                for copy in types.enum_copies(decl) {
                    emitter.copied_enums.insert(copy.name.clone());
                    emitter.register_enum(copy, statement.span)?;
                }
            }
            StmtKind::Enum(decl) => emitter.register_enum(decl, statement.span)?,
            _ => {}
        }
    }

    emitter.top_functions = emitter.functions.clone();

    // The top of the file becomes one function, `zeno_program`. Each
    // function it passes on the way is put aside, and written afterwards.
    for statement in &program.statements {
        emitter.statement(statement)?;
    }
    // The tests run after the program, and see what it made. (With a
    // resource at the top of the file, they ran before it was released.)
    if let Some(tests) = tests {
        if !emitter.code.ended && !emitter.tests_run {
            emitter.run_tests(tests)?;
        }
    }
    // The end of the file is the end of the outermost block.
    if !emitter.code.ended {
        emitter.free_owned(0)?;
    }
    // `noinline` keeps it a function of its own. The collector looks for
    // addresses on the stack below `main`, and this is where they are.
    emitter.program = emitter.finish("define internal void @zeno_program() noinline".to_string());
    while let Some(waiting) = emitter.waiting.pop_front() {
        emitter.function(waiting)?;
    }

    Ok(emitter.file(file, &program.modules))
}

impl Emitter<'_> {
    /// Put the finished parts together.
    fn file(mut self, file: &str, modules: &[ModuleFile]) -> String {
        let name = self.string(file);
        // `zeno test` names the file before it runs it.
        let named = self.tests.is_some().then(|| self.string(&format!("{file}\n")));
        if named.is_some() {
            self.used.insert("zeno_test_out");
            self.used.insert("zeno_tests_end");
        }
        // The files the program imports, for an error that stops the
        // program in one of them.
        let modules: Vec<String> = modules.iter().map(|module| format!("ptr {}", self.string(&module.path))).collect();
        let mut out = format!("; {file}, compiled by `zeno build --llvm`.\n");
        out += "; The runtime it calls is in the C file written beside this one.\n\n";

        // The shapes of the program's data: what is in an object of each
        // class, and in a value of each enum case. A list is the
        // runtime's: how many items, room for how many, and where they are.
        out += "%zeno_list = type { i64, i64, ptr }\n";
        for shape in &self.structs {
            out += shape;
            out.push('\n');
        }
        out.push('\n');

        // The text the program mentions. `[6 x i8]` is six bytes: the
        // characters and the zero byte that ends a C string.
        for (text, name) in &self.strings {
            let mut bytes = String::new();
            for byte in text.bytes() {
                match byte {
                    b'"' | b'\\' | 0..=0x1f | 0x7f.. => bytes += &format!("\\{byte:02X}"),
                    _ => bytes.push(byte as char),
                }
            }
            out += &format!("{name} = private unnamed_addr constant [{} x i8] c\"{bytes}\\00\"\n", text.len() + 1);
        }
        out.push('\n');

        // The variables at the top of the file, and the values that are
        // made once and never change.
        for global in &self.globals {
            out += global;
            out.push('\n');
        }
        if !self.globals.is_empty() {
            out.push('\n');
        }

        // What lives in the runtime.
        out += "@zeno_file = external global ptr\n";
        if !modules.is_empty() {
            out += "@zeno_modules = external global ptr\n";
            out += &format!("@zeno.modules = private constant [{} x ptr] [{}]\n", modules.len(), modules.join(", "));
        }
        out += "@zeno_raised = external global ptr\n";
        if self.used.contains("zeno_raised_line") {
            // Where the error on its way was raised: kept aside while a
            // resource is released (decision 59.1).
            out += "@zeno_raised_line = external global i32\n";
            out += "@zeno_raised_col = external global i32\n";
        }
        if self.collector {
            out += "@zeno_stack_base = external global ptr\n";
            self.used.insert("zeno_mark_words");
        }
        if !self.collector {
            // How much of the text room is in use. See `text_end`.
            out += "@zeno_text_top = external global i64\n";
        }
        if self.memory {
            // `atexit` is C's "call this when the program ends".
            out += "declare i32 @atexit(ptr)\n";
            out += "declare void @zeno_memory_report()\n";
        }
        for (name, declaration) in DECLARED {
            if self.used.contains(name) {
                let for_firmware = DECLARED_FOR_FIRMWARE.iter().find(|(other, _)| self.firmware && other == name);
                out += for_firmware.map_or(declaration, |(_, declaration)| declaration);
                out.push('\n');
            }
        }
        out.push('\n');
        for (name, written) in WRITTEN {
            if self.used.contains(name) {
                out += written;
                out.push('\n');
            }
        }

        // The functions the compiler wrote for itself, then the program's
        // own as they come in the program, then the top of the file.
        for definition in self.helpers.iter().chain(&self.definitions) {
            out += definition;
            out.push('\n');
        }
        out += &self.program;
        out.push('\n');

        // The collector calls this to be shown every global that may hold
        // the address of something it looks after. Each is one word (8
        // bytes) wide.
        if self.collector {
            out += "define void @zeno_roots() {\nentry:\n";
            out += "  call void @zeno_mark_words(ptr @zeno_raised, i64 8)\n";
            for root in &self.roots {
                out += &format!("  call void @zeno_mark_words(ptr {root}, i64 8)\n");
            }
            out += "  ret void\n}\n\n";
        }

        // Where the program starts. `%base` is a variable of `main`, so
        // its address marks where the stack begins, for the collector.
        // `main` takes the command line only in a program that reads it.
        let arguments = self.used.contains("zeno_program_start");
        out += if arguments { "define i32 @main(i32 %argc, ptr %argv) {\nentry:\n" } else { "define i32 @main() {\nentry:\n" };

        if self.collector {
            out += "  %base = alloca ptr\n";
            out += "  store ptr %base, ptr @zeno_stack_base\n";
        }
        if self.memory {
            out += "  %asked = call i32 @atexit(ptr @zeno_memory_report)\n";
        }
        out += &format!("  store ptr {name}, ptr @zeno_file\n");
        if !modules.is_empty() {
            out += "  store ptr @zeno.modules, ptr @zeno_modules\n";
        }
        if arguments {
            out += "  call void @zeno_program_start(i32 signext %argc, ptr %argv)\n";
        }
        if let Some(named) = &named {
            out += &format!("  call void @zeno_test_out(ptr {named})\n");
        }
        out += "  call void @zeno_program()\n";
        out += match named {
            Some(_) => "  %failed = call i32 @zeno_tests_end()\n  ret i32 %failed\n}\n",
            None => "  ret i32 0\n}\n",
        };
        out
    }

    // ------------------------------------------------- Writing instructions

    /// Write one instruction into the block being filled.
    fn ins(&mut self, text: impl AsRef<str>) {
        self.open();
        self.code.lines.push(format!("  {}", text.as_ref()));
    }

    /// Write an instruction that gives a value, and return the new name
    /// for that value.
    fn set(&mut self, text: impl AsRef<str>) -> String {
        self.code.temps += 1;
        let temp = format!("%t{}", self.code.temps);
        self.ins(format!("{temp} = {}", text.as_ref()));
        temp
    }

    /// A number for a new group of labels, so that `then4`, `else4` and
    /// `end4` can be seen to belong together.
    fn number(&mut self) -> usize {
        self.code.labels += 1;
        self.code.labels
    }

    /// Write the instruction that ends the block.
    fn end(&mut self, text: impl AsRef<str>) {
        self.ins(text);
        self.code.ended = true;
    }

    /// Leave the block for another one, unless it has been left already
    /// (by a `return`, say).
    fn jump(&mut self, to: &str) {
        if !self.code.ended {
            self.end(format!("br label %{to}"));
        }
    }

    /// Leave the block for one of two others.
    fn jump_if(&mut self, test: &str, yes: &str, no: &str) {
        self.end(format!("br i1 {test}, label %{yes}, label %{no}"));
    }

    /// Start the block with this label. A block cannot simply run on into
    /// the next one, so if the one before is still open it jumps here.
    fn place(&mut self, label: &str) {
        self.jump(label);
        self.code.lines.push(String::new());
        self.code.lines.push(format!("{label}:"));
        self.code.block = label.to_string();
        self.code.ended = false;
    }

    /// Make sure there is a block to write into. After a `return` there
    /// is none, and what follows can never run. It still has to be valid
    /// IR, so it gets a block of its own that nothing jumps to. LLVM
    /// throws such a block away.
    fn open(&mut self) {
        if self.code.ended {
            let label = format!("dead{}", self.number());
            self.place(&label);
        }
    }

    /// Call something from `DECLARED` or `WRITTEN` and return its result.
    fn call(&mut self, returns: &str, name: &'static str, args: impl AsRef<str>) -> String {
        // `--fast`: a float `/` is `fdiv` and `math.sqrt` is LLVM's square
        // root, with no check in front. The arguments are `double a,
        // double b, i32 line, i32 col`; a value never holds ", ".
        if self.fast && (name == "zeno.div" || name == "zeno.sqrt") {
            let parts: Vec<&str> = args.as_ref().split(", ").collect();
            if name == "zeno.div" {
                let right = parts[1].trim_start_matches("double ");
                return self.set(format!("fdiv {}, {right}", parts[0]));
            }
            self.uses("llvm.sqrt.f64");
            return self.set(format!("call double @llvm.sqrt.f64({})", parts[0]));
        }
        self.uses(name);
        self.set(format!("call {returns} @{name}({})", args.as_ref()))
    }

    /// The same, for one that gives nothing back.
    fn run(&mut self, name: &'static str, args: impl AsRef<str>) {
        self.uses(name);
        self.ins(format!("call void @{name}({})", args.as_ref()));
    }

    fn uses(&mut self, name: &'static str) {
        let known = DECLARED.iter().chain(WRITTEN).any(|(declared, _)| *declared == name);
        debug_assert!(known, "{name} is not declared");
        self.used.insert(name);
        // The ones written in IR lean on the runtime in their turn.
        if name == "zeno.sqrt" {
            self.used.insert("zeno_sqrt");
            self.used.insert("llvm.sqrt.f64");
        } else if name.starts_with("zeno.box.keep.") {
            self.used.insert("zeno_alloc");
        } else if let Some(scalar) = name.strip_prefix("zeno.box.take.") {
            self.used.insert(box_function("unbox", scalar));
            self.used.insert("zeno_free");
        } else if let Some(scalar) = name.strip_prefix("zeno.list.keep.boxes.") {
            self.used.insert(box_function("unbox", scalar));
            self.uses(box_function("box.keep", scalar));
        } else if name == "zeno.list.drop.boxes" {
            self.used.insert("zeno_free");
            self.used.insert("zeno_list_drop");
        } else if name.starts_with("zeno.box") {
            self.used.insert("zeno_alloc_text");
        } else if name.starts_with("zeno.") && !name.starts_with("zeno.unbox") {
            self.used.insert("zeno_by_zero");
            self.used.insert("zeno_overflow");
        }
    }

    /// The name of a constant holding `text`.
    fn string(&mut self, text: &str) -> String {
        if let Some((_, name)) = self.strings.iter().find(|(known, _)| known == text) {
            return name.clone();
        }
        let name = format!("@.str.{}", self.strings.len() + 1);
        self.strings.push((text.to_string(), name.clone()));
        name
    }

    /// A new slot on the stack of the function being written.
    fn slot(&mut self, name: String, ty: &str) -> String {
        self.code.slots.push(format!("  {name} = alloca {ty}"));
        self.code.on_stack.insert(name.clone());
        name
    }

    /// Read what a slot holds.
    ///
    /// In a function with a `try` in it, the function's own slots are
    /// read and written with `volatile`, which tells LLVM to leave them
    /// in memory and touch them exactly when the program says. Otherwise
    /// LLVM would keep a `var` in a processor register, and the jump that
    /// brings an error back to its `try` puts the registers back as they
    /// were when the `try` began: the `except` block would see the `var`
    /// as it was then. C has the same rule and the same word for it; see
    /// `changed_in_try` in `emit_c`.
    fn load(&mut self, ty: &str, slot: &str) -> String {
        let how = self.how(slot);
        self.set(format!("load{how} {ty}, ptr {slot}"))
    }

    /// Write a value into a slot.
    fn store(&mut self, ty: &str, value: &str, slot: &str) {
        let how = self.how(slot);
        self.ins(format!("store{how} {ty} {value}, ptr {slot}"));
    }

    fn how(&self, slot: &str) -> &'static str {
        if self.code.careful && self.code.on_stack.contains(slot) {
            " volatile"
        } else {
            ""
        }
    }

    /// The address of one field of a struct. `getelementptr` ("get
    /// element pointer", GEP for short) does no reading: it is the sum
    /// "where the struct starts, plus how far in this field is", which
    /// LLVM works out from the type. The first `i32 0` says "the struct
    /// at this address itself", the second number picks the field.
    fn field_at(&mut self, ty: &str, object: &str, index: usize) -> String {
        self.set(format!("getelementptr {ty}, ptr {object}, i32 0, i32 {index}"))
    }

    /// Read field `index` of the object of `class` at `object`.
    ///
    /// A sized field is read at its own width and then made an `i64`,
    /// which is what a whole number is everywhere outside an object.
    /// `zext` ("zero extend") fills the new bits with zeros, right for a
    /// `u8`. `sext` ("sign extend") fills them with copies of the top
    /// bit, so that an `i8` holding -1 (eight ones) becomes sixty-four
    /// ones, which is -1 again.
    fn load_field(&mut self, class: &str, object: &str, index: usize, span: Span) -> Result<String, Error> {
        let (ty, kind) = (self.classes[class].ty.clone(), self.classes[class].fields[index].1.clone());
        let at = self.field_at(&ty, object, self.classes[class].at(index));
        Ok(match self.classes[class].sized[index] {
            Some(width) => {
                let small = self.set(format!("load {}, ptr {at}", width.ir_type()));
                let extend = if width.signed { "sext" } else { "zext" };
                self.set(format!("{extend} {} {small} to i64", width.ir_type()))
            }
            None => self.set(format!("load {}, ptr {at}", ir_type(&kind, span)?)),
        })
    }

    /// Put `value` in field `index` of the object of `class` at `object`.
    /// For a sized field, `trunc` ("truncate") keeps the low bits. The
    /// checker made sure the number is in range, so none are lost.
    fn store_field(&mut self, class: &str, object: &str, index: usize, value: &str, span: Span) -> Result<(), Error> {
        let (ty, kind) = (self.classes[class].ty.clone(), self.classes[class].fields[index].1.clone());
        let at = self.field_at(&ty, object, self.classes[class].at(index));
        match self.classes[class].sized[index] {
            Some(width) => {
                let small = self.set(format!("trunc i64 {value} to {}", width.ir_type()));
                self.ins(format!("store {} {small}, ptr {at}", width.ir_type()));
            }
            None => self.ins(format!("store {} {value}, ptr {at}", ir_type(&kind, span)?)),
        }
        Ok(())
    }

    /// Finish the function being written and return its text.
    fn finish(&mut self, first_line: String) -> String {
        if !self.code.ended && self.code.fallible {
            // The end of a function that can raise, reached with no
            // `return`: it worked.
            match (self.code.returns.clone(), ir_type(&self.code.returns, Span { line: 0, col: 0 })) {
                (Kind::None, _) => self.end("ret i1 true"),
                (Kind::Optional(_), Ok(ty)) => {
                    self.ins(format!("store {ty} {}, ptr %result", zero(ty)));
                    self.end("ret i1 true");
                }
                _ => self.end("unreachable"),
            }
        }
        if !self.code.ended {
            match (self.code.returns.clone(), ir_type(&self.code.returns, Span { line: 0, col: 0 })) {
                (Kind::None, _) => self.end("ret void"),
                // A function that may give back `none` does so if it
                // reaches its end without a `return`.
                (Kind::Optional(_), Ok(ty)) => self.end(format!("ret {ty} {}", zero(ty))),
                // The checker made sure every other path gives a value
                // back, so the end of the body is never reached.
                _ => self.end("unreachable"),
            }
        }
        let code = std::mem::replace(&mut self.code, Code::new(Kind::None, false));
        let mut text = format!("{first_line} {{\nentry:\n");
        for line in code.slots.iter().chain(&code.lines) {
            text += line;
            text.push('\n');
        }
        text += "}\n";
        text
    }

    /// Write a function of the compiler's own (one that shows an object,
    /// say), putting aside the function that was being written and taking
    /// it up again afterwards. `write` must end the function itself.
    fn side(&mut self, first_line: String, write: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        let code = std::mem::replace(&mut self.code, Code::new(Kind::None, true));
        let scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        let done = write(self);
        let text = self.finish(first_line);
        self.code = code;
        self.scopes = scopes;
        done?;
        self.helpers.push(text);
        Ok(())
    }

    // ------------------------------------------- Functions, classes, enums

    /// What a call to this function or method needs to know.
    fn entry(&self, decl: &Arc<Function>, name: String, span: Span) -> Result<Entry, Error> {
        let Some(kind) = self.types.function(decl) else {
            return Err(cannot(format!("`{}`, whose types could not be worked out", decl.name), span));
        };
        let owned = kind.params.iter().map(|param| matches!(param, Kind::Owned(_))).collect();
        let kind = FunctionKind {
            params: kind.params.iter().map(shared).collect(),
            returns: shared(&kind.returns),
            raises: kind.raises.clone(),
        };
        for param in &kind.params {
            ir_type(param, span)?;
        }
        return_type(&kind.returns, span)?;
        let lends = self.types.looks(decl);
        Ok(Entry { decl: decl.clone(), kind, name, owned, lends, within: self.within, sight: Vec::new() })
    }

    /// A class becomes a struct type: its fields, in the order they are
    /// listed. `%z_Point = type { double, double }`.
    fn register_class(&mut self, decl: &Class, span: Span) -> Result<(), Error> {
        let Some(kinds) = self.types.class(decl) else {
            return Err(cannot(format!("the class `{}`", decl.name), span));
        };
        let owned_fields = kinds.iter().map(|kind| self.types.owned_class(decl) && matches!(kind, Kind::Owned(_))).collect();
        let kinds: Vec<Kind> = kinds.iter().map(shared).collect();
        let ty = named('%', "z_", &decl.name);
        let mut sized = self.types.widths(decl).cloned().unwrap_or_default();
        sized.resize(kinds.len(), None);
        let mut inside = Vec::new();
        for (kind, width) in kinds.iter().zip(&sized) {
            // `%z_Packet = type { i8, i16, ptr }`: a u8 and a u16 take
            // three bytes between them and not sixteen.
            inside.push(match width {
                Some(width) => width.ir_type(),
                None => ir_type(kind, span)?,
            });
        }
        // An object that is seen as a `Shape` has to say which class it
        // really is, so that `shape.area()` can find the right `area`.
        // It carries its class's number in front of its fields.
        let mut number = None;
        if decl.interface || !decl.is.is_empty() {
            inside.insert(0, "i32");
        }
        if !decl.is.is_empty() {
            self.numbered += 1;
            number = Some(self.numbered);
            for interface in &decl.is {
                let interface = self.types.interface_name(interface);
                self.is_a.entry(interface).or_default().push((self.numbered, decl.name.clone()));
            }
        }
        if inside.is_empty() {
            // An object with no fields still needs an address of its own.
            inside.push("i8");
        }
        self.structs.push(format!("{ty} = type {{ {} }}", inside.join(", ")));

        // A method is a function whose first parameter is the object.
        let mut methods = HashMap::new();
        for method in &decl.methods {
            let name = named('@', "z_", &format!("{}.{}", decl.name, method.name));
            methods.insert(method.name.clone(), self.entry(method, name, span)?);
        }
        let fields =
            decl.fields.iter().zip(kinds).map(|(field, kind)| (field.name.clone(), kind, field.default.clone())).collect();
        let entry = ClassEntry { fields, sized, methods, ty, owned_fields, number, interface: decl.interface };
        self.classes.insert(decl.name.clone(), entry);
        Ok(())
    }

    /// Inside a function the compiler writes for an interface: read the
    /// number the object at `object` carries, and leave for the block of
    /// the class with that number. Returns the label of each class's
    /// block, in the order of `classes`. The last class is the `default`
    /// of the `switch`, which has to have one.
    fn switch_on_class(&mut self, object: &str, classes: &[(usize, String)]) -> Vec<String> {
        let Some(((_, _), before)) = classes.split_last() else {
            // No class is this interface, so no object is ever seen as
            // one, and this is never reached.
            self.end("unreachable");
            return Vec::new();
        };
        let n = self.number();
        let number = self.set(format!("load i32, ptr {object}"));
        let labels: Vec<String> = classes.iter().map(|(class, _)| format!("is{n}.{class}")).collect();
        let cases: Vec<String> =
            before.iter().zip(&labels).map(|((class, _), label)| format!("i32 {class}, label %{label}")).collect();
        self.end(format!("switch i32 {number}, label %{} [ {} ]", labels[labels.len() - 1], cases.join(" ")));
        labels
    }

    /// The functions of an interface. Each reads the number the object
    /// carries and passes the call on to the function of that class:
    ///
    /// ```text
    /// define internal double @z_Shape.area(ptr %a0) {
    /// entry:
    ///   %t1 = load i32, ptr %a0
    ///   switch i32 %t1, label %is1.2 [ i32 1, label %is1.1 ]
    /// is1.1:
    ///   %t2 = call double @z_Circle.area(ptr %a0)
    ///   ret double %t2
    /// is1.2:
    ///   %t3 = call double @z_Square.area(ptr %a0)
    ///   ret double %t3
    /// }
    /// ```
    fn pass_on(&mut self, decl: &Class, span: Span) -> Result<(), Error> {
        let classes = self.is_a.get(&decl.name).cloned().unwrap_or_default();
        for method in &decl.methods {
            let entry = self.classes[&decl.name].methods[&method.name].clone();
            // In a firmware build a function that can raise gives back
            // `true` or `false`, and its answer goes out through
            // `%result`: see `function`.
            let fallible = self.firmware && !method.raises.is_empty();
            let gives = entry.kind.returns != Kind::None;
            let mut params = Vec::new();
            for (index, kind) in entry.kind.params.iter().enumerate() {
                params.push(format!("{} %a{index}", ir_type(kind, span)?));
            }
            // The later of several values go out where the caller said.
            params.extend(gives_params(&entry.kind.returns));
            let args = params.join(", ");
            if fallible && gives {
                params.push("ptr %result".to_string());
            }
            let returns = if fallible { "zeroext i1" } else { return_type(&entry.kind.returns, span)? };
            let mut own = Vec::new();
            for (_, class) in &classes {
                let Some(found) = self.classes.get(class).and_then(|entry| entry.methods.get(&method.name)) else {
                    return Err(cannot(format!("the class `{class}` as a `{}`", decl.name), span));
                };
                own.push(found.clone());
            }
            self.side(format!("define internal {returns} {}({})", entry.name, params.join(", ")), |emitter| {
                let blocks = emitter.switch_on_class("%a0", &classes);
                for (block, own) in blocks.iter().zip(&own) {
                    emitter.place(block);
                    let own_fallible = emitter.firmware && !own.decl.raises.is_empty();
                    let ty = return_type(&entry.kind.returns, span)?;
                    match (fallible, own_fallible, gives) {
                        (false, _, true) => {
                            let answer = emitter.set(format!("call {ty} {}({args})", own.name));
                            emitter.end(format!("ret {ty} {answer}"));
                        }
                        (false, _, false) => {
                            emitter.ins(format!("call void {}({args})", own.name));
                            emitter.end("ret void");
                        }
                        (true, true, _) => {
                            let result = if gives { ", ptr %result" } else { "" };
                            let worked = emitter.set(format!("call zeroext i1 {}({args}{result})", own.name));
                            emitter.end(format!("ret i1 {worked}"));
                        }
                        (true, false, true) => {
                            let answer = emitter.set(format!("call {ty} {}({args})", own.name));
                            emitter.ins(format!("store {ty} {answer}, ptr %result"));
                            emitter.end("ret i1 true");
                        }
                        (true, false, false) => {
                            emitter.ins(format!("call void {}({args})", own.name));
                            emitter.end("ret i1 true");
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// An enum value is the address of a small block that starts with a
    /// number saying which case it is (its "tag"), followed by whatever
    /// that case carries. Each case that carries something has a struct
    /// type of its own: `%z_Shape.Circle = type { i64, double }`.
    ///
    /// In C the cases share one `union`, as big as the biggest. IR has no
    /// unions and needs none: every block starts with the tag, and once
    /// the tag has been read the right struct type is known.
    fn register_enum(&mut self, decl: &Enum, span: Span) -> Result<(), Error> {
        let Some(kinds) = self.types.enumeration(decl).cloned() else {
            return Err(cannot(format!("the enum `{}`", decl.name), span));
        };
        let mut cases = Vec::new();
        let mut takes = Vec::new();
        for (index, (case, case_kinds)) in decl.variants.iter().zip(&kinds).enumerate() {
            takes.push(case_kinds.iter().map(|kind| matches!(kind, Kind::Owned(_))).collect());
            // A case of a copy of a generic enum is found through the
            // copy the checker noted, since each copy has a `Just`.
            if !self.copied_enums.contains(&decl.name) {
                self.cases.insert(case.name.clone(), (decl.name.clone(), index));
            }
            let fields: Vec<(String, Kind)> =
                case.fields.iter().map(|field| field.name.clone()).zip(case_kinds.iter().map(shared)).collect();
            if fields.is_empty() {
                // A case that carries nothing needs no memory of its own:
                // one fixed value stands for every use of it.
                let value = bare_case(&decl.name, &case.name);
                self.globals.push(format!("{value} = internal constant {{ i64 }} {{ i64 {index} }}"));
            } else {
                let mut inside = vec!["i64"];
                for (_, kind) in &fields {
                    inside.push(ir_type(kind, span)?);
                }
                self.structs.push(format!("{} = type {{ {} }}", case_type(&decl.name, &case.name), inside.join(", ")));
            }
            cases.push((case.name.clone(), fields));
        }
        self.enums.insert(decl.name.clone(), EnumEntry { cases, takes });
        Ok(())
    }

    /// The parameters of a function: as variables for its body, and as
    /// they are written on its first line. A parameter cannot be changed,
    /// so the value that arrives is the variable.
    fn parameters(
        &self,
        decl: &Function,
        kind: &FunctionKind,
        span: Span,
    ) -> Result<(HashMap<String, Variable>, Vec<String>), Error> {
        let mut params = HashMap::new();
        let mut list = Vec::new();
        for (param, kind) in decl.params.iter().zip(&kind.params) {
            let arrives = named('%', "arg.", &param.name);
            list.push(format!("{} {arrives}", ir_type(kind, span)?));
            params.insert(param.name.clone(), Variable { place: Place::Value(arrives), kind: kind.clone(), open: false, owner: false, inner: None });
        }
        Ok((params, list))
    }

    /// Write out one function or method.
    fn function(&mut self, waiting: Waiting) -> Result<(), Error> {
        let Waiting { entry, span, sees, sight } = waiting;
        let mut before = Vec::new();
        for (name, seen) in &sight {
            before.push((name.clone(), self.functions.insert(name.clone(), seen.clone())));
        }
        let seen = std::mem::replace(&mut self.inner_seen, sight);
        let within = std::mem::replace(&mut self.within, Arc::as_ptr(&entry.decl) as usize);
        let done = self.function_in(entry, span, sees);
        self.within = within;
        self.inner_seen = seen;
        for (name, was) in before.into_iter().rev() {
            match was {
                Some(entry) => self.functions.insert(name, entry),
                None => self.functions.remove(&name),
            };
        }
        done
    }

    /// The body of `function`.
    fn function_in(&mut self, entry: Entry, span: Span, sees: HashMap<String, Variable>) -> Result<(), Error> {
        self.code = Code::of(&entry.decl.body, entry.kind.returns.clone(), true, !self.firmware);
        // In a firmware build a function that can raise gives back `true`
        // or `false`, and its answer goes out through `%result`.
        self.code.fallible = self.firmware && !entry.decl.raises.is_empty();
        let fallible = self.code.fallible;
        let (mut params, mut list) = self.parameters(&entry.decl, &entry.kind, span)?;
        // A parameter that takes over an owned value is an owner like any
        // other: the function frees the value when it ends, unless it has
        // passed it on by then. So it is moved into a slot, where giving
        // it away can leave null behind.
        for (index, param) in entry.decl.params.iter().enumerate() {
            if entry.owned.get(index) == Some(&true) {
                let kind = entry.kind.params[index].clone();
                let ty = ir_type(&kind, span)?;
                let slot = self.stack_slot(&param.name, ty);
                self.store(ty, &named('%', "arg.", &param.name), &slot);
                params.insert(param.name.clone(), Variable { place: Place::Slot(slot), kind, open: false, owner: true, inner: None });
            }
        }
        let outer = std::mem::replace(&mut self.scopes, vec![sees, params]);
        let built_in = |name: &str| self.lookup(name).is_none() && !self.functions.contains_key(name);
        // A parameter that may be `none` is read only after a test that it
        // is not, so its fields cannot be read at the start.
        let may_be_none = |name: &str| matches!(self.lookup(name), Some(Variable { kind: Kind::Optional(_), .. }));
        let params = entry.decl.params.iter().map(|param| param.name.as_str()).filter(|name| !may_be_none(name));
        let once = crate::ast::fields_read_once(params, &entry.decl.body, &built_in)
            .map(|(params, written)| (params.into_iter().collect(), written, HashMap::new()));
        let once = std::mem::replace(&mut self.once, once);
        let mut done = self.block(&entry.decl.body);
        self.once = once;
        if done.is_ok() && !self.code.ended {
            // The end of the body: free what the parameters still own.
            done = self.free_owned(1);
        }
        self.scopes = outer;
        done?;

        // `internal` is what `static` is in C: nothing outside this file
        // needs the function, so LLVM may inline it and drop it.
        let mut returns = return_type(&entry.kind.returns, span)?;
        list.extend(gives_params(&entry.kind.returns));
        if fallible {
            if entry.kind.returns != Kind::None {
                list.push("ptr %result".to_string());
            }
            returns = "zeroext i1";
        }
        let text = self.finish(format!("define internal {returns} {}({})", entry.name, list.join(", ")));
        self.definitions.push(text);
        Ok(())
    }

    /// The Zeno type written in the source, as a kind.
    fn kind_of_type(&self, ty: &Type) -> Option<Kind> {
        // Inside a copy of a generic, `T` is whatever the copy was made
        // for, and `Stack[int]` is a copy itself. Only the checker knows
        // those, so its answer is used where it has one.
        if let Some(kind) = self.types.annotation(ty) {
            return Some(emit_c::disowned(kind.clone()));
        }
        let (name, args) = match ty {
            Type::Named { name, args } => (name, args),
            // Who owns a value does not change how it is held.
            Type::Owned(inner) | Type::Take(inner) => return self.kind_of_type(inner),
            Type::Length(_) => return None,
            Type::Optional(inner) => return Some(Kind::Optional(Box::new(self.kind_of_type(inner)?))),
            Type::Several(types) => {
                return Some(Kind::Several(types.iter().map(|ty| self.kind_of_type(ty)).collect::<Option<Vec<Kind>>>()?))
            }
            Type::Function { params, returns, raises } => {
                let params = params.iter().map(|param| self.kind_of_type(param)).collect::<Option<Vec<Kind>>>()?;
                let returns = match returns {
                    Some(returns) => self.kind_of_type(returns)?,
                    None => Kind::None,
                };
                let raises = raises.iter().map(|kind| kind.to_string()).collect();
                return Some(Kind::Function(Box::new(FunctionKind { params, returns, raises })));
            }
        };
        Some(match (name.as_str(), args.as_slice()) {
            ("int", []) => Kind::Int,
            ("float", []) => Kind::Float,
            ("str", []) => Kind::Str,
            ("bool", []) => Kind::Bool,
            ("list", [item]) | ("list", [item, Type::Length(_)]) => match self.kind_of_type(item)? {
                // A list of bytes keeps one byte to an item (36.2).
                Kind::Int if matches!(item, Type::Named { name, .. } if name == "u8") => Kind::List(Box::new(Kind::Byte)),
                item => Kind::List(Box::new(item)),
            },
            ("dict", [key, value]) => {
                Kind::Dict(Box::new(self.kind_of_type(key)?), Box::new(self.kind_of_type(value)?))
            }
            (name, []) if self.classes.contains_key(name) => Kind::Class(name.to_string()),
            (name, []) if self.enums.contains_key(name) => Kind::Enum(name.to_string()),
            (name, []) if units::is_unit(name) => Kind::Quantity(name.to_string()),
            ("Pin", []) => Kind::Pin,
            ("InputPin", []) => Kind::InputPin,
            ("Serial", []) => Kind::Serial,
            ("AnalogPin", []) => Kind::AnalogPin,
            ("PwmPin", []) => Kind::PwmPin,
            ("Ticker", []) => Kind::Ticker,
            ("Interrupt", []) => Kind::Interrupt,
            // `rising`, `falling` and `either` are the strs of their names.
            ("Edge", []) => Kind::Str,
            // `pull_up` and `pull_down` are the strs of their names.
            ("Pull", []) => Kind::Str,
            ("pointer", [Type::Named { name, args }]) if args.is_empty() => Kind::Pointer(Pointee::named(name)?),
            ("Register", [Type::Named { name, args }]) if args.is_empty() => Kind::Register(ints::width(name)?),
            // `u8` and the rest: an int that is known to be small.
            (name, []) if ints::width(name).is_some() => Kind::Int,
            _ => return None,
        })
    }

    // ----------------------------------------------------------- Statements

    fn block(&mut self, statements: &[Stmt]) -> Result<(), Error> {
        self.scopes.push(HashMap::new());
        let mut done = self.statements(statements);
        // The block has reached its end. Unless its last statement left
        // it (and freed on its way out), free what its owners hold.
        if done.is_ok() && !self.code.ended {
            done = self.free_owned(self.scopes.len() - 1);
        }
        self.scopes.pop();
        done
    }

    /// The statements of a block, in order, with the functions with names
    /// that it declares, as `emit_c` places them (decision 56.16).
    fn statements(&mut self, statements: &[Stmt]) -> Result<(), Error> {
        if self.at_the_top() {
            return statements.iter().try_for_each(|statement| self.statement(statement));
        }
        // What follows the `let` of a resource is more of the same block
        // (decision 59.1): its functions are looked for there too, and
        // the places they are made at are counted through it.
        let flat = crate::ast::flat(statements);
        let made = self.inner_functions(&flat)?;
        let done = self.statements_from(statements, &made, 0, flat.len());
        let frame = self.inner_frames.pop().unwrap_or_default();
        for (name, before) in frame.into_iter().rev() {
            match before {
                Some(entry) => self.functions.insert(name, entry),
                None => self.functions.remove(&name),
            };
        }
        done
    }

    /// The statements of `statements`, the first of them at `first` in
    /// the block read flat, and the rest of a resource's block inside
    /// what releases it. `total` is how many the block has, read flat.
    fn statements_from(
        &mut self,
        statements: &[Stmt],
        made: &[(usize, Arc<Function>, Span)],
        first: usize,
        total: usize,
    ) -> Result<(), Error> {
        for (offset, statement) in statements.iter().enumerate() {
            let index = first + offset;
            if let StmtKind::Scoped { name, body } = &statement.kind {
                // Its first statement is the one at `index`, read flat.
                self.open();
                return self.scoped(name, body, statement.span, &mut |this| this.statements_from(body, made, index, total));
            }
            for (at, decl, span) in made {
                if *at == index {
                    self.inner_closure(decl, *span)?;
                }
            }
            self.statement(statement)?;
        }
        for (at, decl, span) in made {
            if *at == total && !self.code.ended {
                self.inner_closure(decl, *span)?;
            }
        }
        Ok(())
    }

    /// The rest of a block after the `let` of a resource, or the body of
    /// a function that takes one (decision 59.1), as `emit_c::Emitter::
    /// scoped` writes it. However it is left, the resource is released if
    /// its variable still holds it, and the leaving goes on as it was. It
    /// opens no block: what is declared in it is the block's. The end of
    /// it, `return`, `break`, `continue` and an error as a value release
    /// it through `free_owned`. In an ordinary build the rest is a `try`
    /// too, for an error that jumps:
    ///
    /// ```text
    ///   call void @zeno_try_open(ptr %try.1)
    ///   %t2 = call i32 @_setjmp(ptr %try.1) returns_twice
    ///   %t3 = icmp eq i32 %t2, 0
    ///   br i1 %t3, label %held1, label %passing1
    /// passing1:                                  ; an error on its way
    ///   ...release...
    ///   call void @zeno_raise_again()
    ///   unreachable
    /// held1:
    ///   ...the rest of the block...
    ///   call void @zeno_try_close(ptr %try.1)
    ///   ...release...
    /// ```
    fn scoped(
        &mut self,
        name: &str,
        body: &[Stmt],
        span: Span,
        compile: &mut dyn FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let top = self.at_the_top();
        let Some(depth) = self.scopes.iter().rposition(|scope| scope.contains_key(name)) else {
            return Err(cannot(format!("the resource `{name}`"), span));
        };
        let mut place = None;
        if !self.firmware {
            let n = self.number();
            let at = format!("%try.{n}");
            self.uses("zeno_try_size");
            self.code.slots.push(format!("  %try.size.{n} = call i64 @zeno_try_size()"));
            self.code.slots.push(format!("  {at} = alloca i8, i64 %try.size.{n}, align 16"));
            self.run("zeno_try_open", format!("ptr {at}"));
            self.uses("_setjmp");
            let came = self.set(format!("call i32 @_setjmp(ptr {at}{SETJMP_TAKES}) returns_twice"));
            let first = self.set(format!("icmp eq i32 {came}, 0"));
            let (held, passing) = (format!("held{n}"), format!("passing{n}"));
            self.jump_if(&first, &held, &passing);
            // The runtime has taken this `try` off its list by now.
            self.place(&passing);
            let kept = self.keep_raised();
            self.release(depth, name, span)?;
            self.put_back_raised(kept);
            self.run("zeno_raise_again", "");
            self.end("unreachable");
            self.place(&held);
            self.code.tries.push(at.clone());
            // A `raise` inside frees nothing of the resource's block:
            // the release above still needs what it holds.
            self.code.try_scopes.push(self.scopes.len());
            place = Some(at);
        }
        self.code.resources.push((depth, name.to_string()));
        let result = compile(self);
        self.code.resources.pop();
        if place.is_some() {
            self.code.try_scopes.pop();
            self.code.tries.pop();
        }
        result?;
        // Left by its last line, which released on its way out.
        if self.code.ended {
            return Ok(());
        }
        if let Some(at) = &place {
            self.run("zeno_try_close", format!("ptr {at}"));
        }
        // Under `zeno test --build` the top of the file ends after its
        // last test, so the tests run before its last resource is
        // released (rulings of decision 59.1).
        let innermost = !matches!(body.last().map(|statement| &statement.kind), Some(StmtKind::Scoped { .. }));
        if top && innermost && !self.tests_run {
            if let Some(tests) = self.tests.clone() {
                self.tests_run = true;
                self.run_tests(&tests)?;
            }
        }
        self.release(depth, name, span)
    }

    /// Release the resource `name` if its variable still holds it: the
    /// variable is left holding nothing first, so that nothing releases
    /// it twice, then `release` is called, and the object is freed after,
    /// unless `release` took it.
    ///
    /// ```text
    ///   %t1 = load ptr, ptr %var.held
    ///   %t2 = icmp eq ptr %t1, null
    ///   br i1 %t2, label %released1, label %release1
    /// release1:
    ///   store ptr null, ptr %var.held
    ///   call void @z_Held.release(ptr %t1)
    ///   call void @zeno.drop.Held(ptr %t1)
    ///   br label %released1
    /// released1:
    /// ```
    /// `depth` is the block the variable is in: a block inside may have
    /// a variable of the same name.
    fn release(&mut self, depth: usize, name: &str, span: Span) -> Result<(), Error> {
        let Some(variable) = self.scopes.get(depth).and_then(|scope| scope.get(name)).cloned() else {
            return Err(cannot(format!("the resource `{name}`"), span));
        };
        let Place::Slot(slot) = &variable.place else {
            return Err(cannot(format!("the resource `{name}`, held where it cannot be given away"), span));
        };
        let entry = match &variable.kind {
            Kind::Class(class) => self.classes.get(class).and_then(|entry| entry.methods.get("release")).cloned(),
            _ => None,
        };
        let Some(entry) = entry else {
            return Err(cannot(format!("the resource `{name}`, with no `release`"), span));
        };
        let held = self.load("ptr", slot);
        let gone = self.set(format!("icmp eq ptr {held}, null"));
        let n = self.number();
        let (release, released) = (format!("release{n}"), format!("released{n}"));
        self.jump_if(&gone, &released, &release);
        self.place(&release);
        self.store("ptr", "null", slot);
        self.ins(format!("call void {}(ptr {held})", entry.name));
        if entry.owned.first() != Some(&true) {
            self.free(&variable.kind, &held, span)?;
        }
        self.place(&released);
        Ok(())
    }

    /// Before a resource is released while an error is on its way: keep
    /// the error aside, for a `release` may catch one of its own inside,
    /// which is noted in the same place.
    fn keep_raised(&mut self) -> (String, String, String) {
        // Variables of the runtime, not functions: see `file`.
        self.used.insert("zeno_raised_line");
        let error = self.set("load ptr, ptr @zeno_raised");
        let line = self.set("load i32, ptr @zeno_raised_line");
        let col = self.set("load i32, ptr @zeno_raised_col");
        (error, line, col)
    }

    /// And after: the error on its way is the one it was.
    fn put_back_raised(&mut self, (error, line, col): (String, String, String)) {
        self.ins(format!("store ptr {error}, ptr @zeno_raised"));
        self.ins(format!("store i32 {line}, ptr @zeno_raised_line"));
        self.ins(format!("store i32 {col}, ptr @zeno_raised_col"));
    }

    /// `free_owned`, for an error on its way: with the error kept aside
    /// while a resource that it releases is released.
    fn free_owned_raised(&mut self, from: usize) -> Result<(), Error> {
        if !self.code.resources.iter().any(|(at, _)| *at >= from) {
            return self.free_owned(from);
        }
        let kept = self.keep_raised();
        self.free_owned(from)?;
        self.put_back_raised(kept);
        Ok(())
    }

    /// The functions with names that a block declares, sorted as
    /// `emit_c::Emitter::inner_functions` sorts them: one that uses
    /// nothing of the function around it waits to be written as a
    /// function of the file, and is in sight from here; one that does is
    /// given back with the statement it is made before.
    fn inner_functions(&mut self, statements: &[&Stmt]) -> Result<Vec<(usize, Arc<Function>, Span)>, Error> {
        let around: HashSet<String> = self.scopes[1..].iter().flat_map(|scope| scope.keys().cloned()).collect();
        let plan = emit_c::inner_plan(statements, &around, "`zeno build --llvm`")?;
        self.inner_frames.push(Vec::new());
        let mut jobs = Vec::new();
        for (decl, span) in &plan.plain {
            self.inner_count += 1;
            let entry = self.entry(decl, named('@', &format!("zeno.inner.{}.", self.inner_count), &decl.name), *span)?;
            let before = self.functions.insert(decl.name.clone(), entry);
            self.inner_frames.last_mut().expect("pushed above").push((decl.name.clone(), before));
            jobs.push((decl.name.clone(), *span));
        }
        let mut seen = self.inner_seen.clone();
        for frame in &self.inner_frames {
            for (name, _) in frame {
                seen.retain(|(other, _)| other != name);
                seen.push((name.clone(), self.functions[name].clone()));
            }
        }
        for (name, span) in jobs {
            let mut entry = self.functions[&name].clone();
            entry.sight = seen.clone();
            self.functions.insert(name, entry.clone());
            self.waiting.push_back(Waiting { entry, span, sees: self.scopes[0].clone(), sight: seen.clone() });
        }
        Ok(plan.closures)
    }

    /// A function with a name written inside a function that uses
    /// variables of the function around it: a closure, held under its
    /// name, which the block owns (decision 56.16).
    fn inner_closure(&mut self, decl: &Arc<Function>, span: Span) -> Result<(), Error> {
        let val = self.closure_made(decl, &decl.name, Some(decl), span)?;
        self.declare(&decl.name, val, false, !self.collector, span)?;
        if let Some(variable) = self.scopes.last_mut().and_then(|scope| scope.get_mut(&decl.name)) {
            variable.inner = Some((decl.clone(), true));
        }
        Ok(())
    }

    /// Is this the top of the file, outside every function and block?
    fn at_the_top(&self) -> bool {
        !self.code.in_function && self.scopes.len() == 1
    }

    fn lookup(&self, name: &str) -> Option<&Variable> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn statement(&mut self, statement: &Stmt) -> Result<(), Error> {
        // The statements that work out one value and are done. In a build
        // with no collector, text they build is given back as they end.
        // (The others give it back at a point of their own choosing.)
        let simple = matches!(
            statement.kind,
            StmtKind::Let { .. } | StmtKind::Const { .. } | StmtKind::Assign { .. } | StmtKind::Expr(_) | StmtKind::Receive { .. }
        );
        let begin = (!self.collector && simple).then(|| self.text_begin());
        // What the statement makes and gives to nobody is freed as it
        // ends, in every build (see `belongs`). A `for` or a `match` keeps
        // such a value until its last block is over.
        let own = self.own_begin();
        let mut result = self.one_statement(statement);
        if result.is_ok() {
            result = self.own_end(own, !matches!(statement.kind, StmtKind::Return(_) | StmtKind::Raise(_)));
        }
        if let Some(mark) = begin.and_then(|begin| self.text_end(begin)) {
            self.text_back(&mark);
        }
        result
    }

    fn one_statement(&mut self, statement: &Stmt) -> Result<(), Error> {
        let span = statement.span;
        self.open();
        match &statement.kind {
            // At the top of the file, where each statement is written by
            // itself, or in a block of statements that `statements` did
            // not read.
            StmtKind::Scoped { name, body } => {
                self.scoped(name, body, span, &mut |this| body.iter().try_for_each(|statement| this.statement(statement)))
            }
            StmtKind::Let { name, ty, value, .. } | StmtKind::Const { name, ty, value } => {
                let mutable = matches!(statement.kind, StmtKind::Let { mutable: true, .. });
                // A type written beside the name settles what `none` and
                // `[]` mean here.
                let expected = ty.as_ref().and_then(|ty| self.kind_of_type(ty));
                let mut val = self.expr_as(value, expected.as_ref())?;
                self.stays(&val, value.span)?;
                val = self.keep(val, is_written(value));
                if let Some(expected) = &expected {
                    val = self.coerce(val, expected, value.span)?;
                }
                if is_empty_list(&val.kind) {
                    return Err(cannot(
                        "an empty list with no type beside it. Write `let items: list[int] = []`",
                        value.span,
                    ));
                }
                if is_empty_dict(&val.kind) {
                    return Err(cannot(
                        "an empty dict with no type beside it. Write `let ages: dict[str, int] = [:]`",
                        value.span,
                    ));
                }
                // The checker knows whether this variable owns its value.
                // With no collector a variable has its own copy of its
                // text, and frees it. Text written in the program is not a
                // copy, but a `var` may be given one later.
                let owner = self.types.owner(statement)
                    || (!self.collector && is_text(&val.kind) && (mutable || !is_written(value)));
                self.declare(name, val, mutable, owner, value.span)
            }
            StmtKind::Assign { target, op, value } => self.assign(target, *op, value, span),
            StmtKind::Receive { declare, places, value } => self.receive(*declare, places, value, span),
            StmtKind::Expr(expr) => {
                let val = self.expr(expr)?;
                if self.types.discarded(expr) {
                    // An owned value that nobody keeps is freed on the spot.
                    self.free(&val.kind, &val.code, span)?;
                }
                Ok(())
            }

            StmtKind::Function(decl) => {
                // One inside a function is made by `statements`.
                if !self.at_the_top() {
                    return Ok(());
                }
                // It is written out later. It may use the variables
                // declared so far and no others, so a copy of them goes
                // with it. A generic is written out once for each copy.
                for decl in copies_of_function(self.types, decl) {
                    let entry = self.functions[&decl.name].clone();
                    self.waiting.push_back(Waiting { entry, span, sees: self.scopes[0].clone(), sight: Vec::new() });
                }
                Ok(())
            }
            StmtKind::Class(decl) => {
                if !self.at_the_top() {
                    return Err(cannot("a class inside a function or block", span));
                }
                // Its methods are functions, and wait like functions.
                for decl in copies_of_class(self.types, decl) {
                    if decl.interface {
                        // Nothing to compile: a function of an interface
                        // passes the call on to the object's class.
                        self.pass_on(decl, span)?;
                        continue;
                    }
                    for method in &decl.methods {
                        let entry = self.classes[&decl.name].methods[&method.name].clone();
                        self.waiting.push_back(Waiting { entry, span, sees: self.scopes[0].clone(), sight: Vec::new() });
                    }
                }
                Ok(())
            }
            StmtKind::Enum(_) if self.at_the_top() => Ok(()),
            StmtKind::Enum(_) => Err(cannot("an enum inside a function or block", span)),

            StmtKind::If { condition, then_block, else_block } => {
                let test = self.condition(condition)?;
                let n = self.number();
                let (then, otherwise, end) = (format!("then{n}"), format!("else{n}"), format!("end{n}"));
                self.jump_if(&test, &then, if else_block.is_some() { &otherwise } else { &end });
                self.place(&then);
                self.block(then_block)?;
                if let Some(else_block) = else_block {
                    // `place` adds the jump from the end of `then` to `end`.
                    self.jump(&end);
                    self.place(&otherwise);
                    self.block(else_block)?;
                }
                self.place(&end);
                Ok(())
            }

            // The checker has seen to what is written inside. In IR it
            // is a block like any other.
            // A build leaves the tests of a program out (decision 10.20).
            // Built for its tests, each test is a function by now, and
            // its asserts are compiled (decision 60.2).
            StmtKind::Assert(condition, message) if self.tests.is_some() => {
                self.test_assert(condition, message.as_ref(), span)
            }
            StmtKind::AssertRaises { error, body } if self.tests.is_some() => self.test_assert_raises(error, body, span),
            StmtKind::Test { .. } | StmtKind::Assert(..) | StmtKind::AssertRaises { .. } => Ok(()),
            StmtKind::Unsafe(body) => {
                let n = self.number();
                let (inside, after) = (format!("unsafe{n}"), format!("safe{n}"));
                self.place(&inside);
                self.block(body)?;
                self.place(&after);
                Ok(())
            }

            StmtKind::While { condition, body } => {
                let n = self.number();
                let (test, body_label, done) = (format!("while{n}"), format!("do{n}"), format!("done{n}"));
                // The test is a block of its own, because it is jumped
                // back to after every trip.
                self.place(&test);
                let holds = self.condition(condition)?;
                self.jump_if(&holds, &body_label, &done);
                self.place(&body_label);
                self.code.loops.push(Loop {
                    next: test.clone(),
                    done: done.clone(),
                    tries: self.code.tries.len(),
                    scopes: self.scopes.len(),
                });
                let result = self.block(body);
                self.code.loops.pop();
                result?;
                self.jump(&test);
                self.place(&done);
                Ok(())
            }

            StmtKind::For { variable, second, iterable, body } => {
                // Text built to find what to loop over is given back
                // before the first trip: see `open_loop`.
                let begin = self.text_begin();
                // A body that cannot change the list walks it in place.
                let built_in = |name: &str| self.lookup(name).is_none() && !self.functions.contains_key(name);
                self.in_place = crate::ast::walks_in_place(iterable, body, &built_in);
                self.text_stays = matches!(&iterable.kind, ExprKind::Name(name) if !crate::ast::mentions(body, name));
                let count = self.open_loop(variable, second.as_deref(), iterable, Some(begin));
                self.in_place = false;
                self.text_stays = false;
                let count = count?;
                self.code.loops.push(Loop {
                    next: count.next.clone(),
                    done: count.done.clone(),
                    tries: self.code.tries.len(),
                    scopes: self.scopes.len(),
                });
                let result = self.block(body);
                self.code.loops.pop();
                self.close_loop(&count);
                result
            }

            StmtKind::Match { subject, arms } => self.match_statement(subject, arms, span),
            StmtKind::Try { body, handlers } => self.try_statement(body, handlers, span),
            StmtKind::Raise(value) => self.raise(value, span),

            StmtKind::Return(value) => {
                if !self.code.in_function {
                    return Err(cannot("`return` outside a function", span));
                }
                let returns = self.code.returns.clone();
                match value {
                    Some(value) if returns != Kind::None => {
                        let begin = self.text_begin();
                        // `return a, b`: each in its turn; the later ones go
                        // out through the pointers the caller passed, each
                        // kept by copy where text is (decision 57). `return
                        // f()` hands f the same pointers.
                        let (mut val, later, written) = match (&value.kind, &returns) {
                            (ExprKind::Several(values), Kind::Several(kinds)) => {
                                let (mut vals, mut held) = (Vec::new(), Vec::new());
                                for (at, (value, kind)) in values.iter().zip(kinds).enumerate() {
                                    let val = self.expr_as(value, Some(kind))?;
                                    self.stays(&val, value.span)?;
                                    let mut val = self.fit(val, kind, span)?;
                                    if at > 0 {
                                        val = self.keep(val, is_written(value));
                                    }
                                    held.extend(self.hold_for_later(&val, at > 0 && !is_written(value), &values[at + 1..]));
                                    vals.push((val, kind.clone()));
                                }
                                for key in &held {
                                    self.let_go(key);
                                }
                                let (first, _) = vals.remove(0);
                                (first, vals, is_written(&values[0]))
                            }
                            _ => {
                                // What was there is put back after: a function written among the
                                // arguments has `return`s of its own (decision 57).
                                let outer = self.outs.replace(gives_params(&returns));
                                let val = self.expr_as(value, Some(&returns));
                                self.outs = outer;
                                let val = val?;
                                self.stays(&val, value.span)?;
                                let mut val = self.fit(val, &returns, span)?;
                                val.kind = emit_c::first_value(&val.kind).clone();
                                (val, Vec::new(), is_written(value))
                            }
                        };
                        let returns = emit_c::first_value(&returns).clone();
                        // With no collector, text that a function gives
                        // back travels in the text room (docs/text.md):
                        // the runtime moves it down to where the room's
                        // top was when this line began.
                        let text = !self.collector && is_text(&returns) && !written;
                        self.code.built |= text;
                        match self.text_end(begin) {
                            Some(mark) if text => {
                                val.code = self.call("ptr", "zeno_text_return", format!("i64 {mark}, ptr {}", val.code));
                            }
                            // The answer has been taken. The text it was
                            // worked out from can be given back.
                            Some(mark) => self.text_back(&mark),
                            None => {}
                        }
                        // The value is worked out while any `try` around
                        // this line is still watching, and before anything
                        // is freed: it may have been read out of what is
                        // freed next. Every block of the function ends
                        // here. Then the `try` is left, and then the
                        // function.
                        self.free_owned(self.base())?;
                        self.leave_tries(0);
                        for (at, (later, kind)) in later.iter().enumerate() {
                            self.ins(format!("store {} {}, ptr %gives.{}", ir_type(kind, span)?, later.code, at + 2));
                        }
                        let ty = ir_type(&returns, span)?;
                        if self.code.fallible {
                            // The answer goes out through the pointer,
                            // and `true` says that it is there.
                            self.ins(format!("store {ty} {}, ptr %result", val.code));
                            self.end("ret i1 true");
                        } else {
                            self.end(format!("ret {ty} {}", val.code));
                        }
                    }
                    Some(value) if !matches!(value.kind, ExprKind::None) => {
                        return Err(cannot("returning a value from a function that returns nothing", span))
                    }
                    _ => {
                        self.free_owned(self.base())?;
                        self.leave_tries(0);
                        self.end(if self.code.fallible { "ret i1 true" } else { "ret void" });
                    }
                }
                Ok(())
            }
            StmtKind::Break | StmtKind::Continue => {
                let Some(Loop { next, done, tries, scopes }) = self.code.loops.last() else {
                    return Err(cannot("`break` or `continue` outside a loop", span));
                };
                let to = if statement.kind == StmtKind::Break { done.clone() } else { next.clone() };
                let (tries, scopes) = (*tries, *scopes);
                // The blocks opened since the loop's body began end here.
                self.free_owned(scopes)?;
                self.leave_tries(tries);
                self.jump(&to);
                Ok(())
            }

            StmtKind::Import { module, names } => {
                let module = module.join(".");
                if module == "web" {
                    return Err(crate::emit_c::for_a_page(span));
                }
                if !matches!(module.as_str(), "math" | "time" | "board" | "files" | "program") {
                    return Err(cannot(format!("the module `{module}`"), span));
                }
                for name in names {
                    self.imported.insert(name.clone(), module.clone());
                }
                Ok(())
            }
        }
    }

    /// `let name = val` or `var name = val`.
    fn declare(&mut self, name: &str, val: Val, mutable: bool, owner: bool, span: Span) -> Result<(), Error> {
        let ty = ir_type(&val.kind, span)?;
        let (place, open) = if self.at_the_top() {
            // Functions use the variables at the top of the file, so
            // those live in memory that is there for the whole run.
            // `internal global` is a `static` variable in C.
            let global = named('@', "z_", name);
            self.globals.push(format!("{global} = internal global {ty} {}", zero(ty)));
            if holds_pointer(&val.kind) {
                self.roots.push(global.clone());
            }
            self.ins(format!("store {ty} {}, ptr {global}", val.code));
            (Place::Slot(global), mutable)
        } else if mutable && self.code.shared.contains(name) && self.collector {
            // A function written inside this one uses the variable too,
            // and either may change it, perhaps long after this function
            // has returned. So it lives in a box of its own that the
            // collector looks after, and both hold the box's address.
            let boxed = self.call("ptr", "zeno_alloc", format!("i64 {}", size_of(ty)));
            self.ins(format!("store {ty} {}, ptr {boxed}", val.code));
            (Place::Slot(boxed), true)
        } else if mutable || owner {
            // A `var` gets a slot on the stack. So does an owner, even one
            // declared with `let`: giving its value away writes null into
            // the slot, and the end of its block frees whatever the slot
            // holds by then.
            let slot = self.stack_slot(name, ty);
            self.store(ty, &val.code, &slot);
            (Place::Slot(slot), false)
        } else {
            // A `let` never changes: the value itself is the variable.
            (Place::Value(val.code), false)
        };
        let variable = Variable { place, kind: val.kind, open, owner, inner: None };
        self.scopes.last_mut().expect("a scope").insert(name.to_string(), variable);
        Ok(())
    }

    /// `let q, r = divide(7, 2)`, `a, b = b, a` (decision 57). All of the
    /// right first, each value kept as a `let` keeps it; then each place,
    /// left to right. A call gives its first value as its answer, and
    /// stores the others in slots whose addresses go after its arguments.
    fn receive(&mut self, declare: Option<bool>, places: &[crate::ast::Place], value: &Expr, span: Span) -> Result<(), Error> {
        let expected: Vec<Option<Kind>> = places
            .iter()
            .map(|place| match place {
                crate::ast::Place::Name { ty: Some(ty), .. } => self.kind_of_type(ty),
                _ => None,
            })
            .collect();
        let dropped = |at: usize| matches!(places.get(at), Some(crate::ast::Place::Drop));
        // Each value, and whether it is a copy of text that this line now
        // owns, which a `_` gives back.
        let mut vals = Vec::new();
        match &value.kind {
            ExprKind::Several(values) => {
                let mut held = Vec::new();
                for (at, value) in values.iter().enumerate() {
                    let val = self.expr_as(value, expected.get(at).and_then(Option::as_ref))?;
                    let val = match dropped(at) {
                        true => val,
                        false => {
                            self.stays(&val, value.span)?;
                            self.keep(val, is_written(value))
                        }
                    };
                    held.extend(self.hold_for_later(&val, !dropped(at) && !is_written(value), &values[at + 1..]));
                    vals.push((val, false));
                }
                for key in &held {
                    self.let_go(key);
                }
            }
            _ => {
                let n = self.number();
                let slots: Vec<String> = (1..places.len()).map(|at| format!("%gives.{n}.{}", at + 1)).collect();
                // The kinds are known once the call is written, and the
                // slots are made before it: each holds 16 bytes, the most
                // a value takes (a number that may be `none`, with its flag).
                for slot in &slots {
                    self.slot(slot.clone(), "[2 x i64]");
                }
                // What was there is put back after: a function written among the
                // arguments has `return`s of its own (decision 57).
                let outer = self.outs.replace(slots.iter().map(|slot| format!("ptr {slot}")).collect());
                let val = self.expr(value);
                self.outs = outer;
                let mut val = val?;
                let Kind::Several(kinds) = val.kind.clone() else {
                    return Err(cannot("this on the right of several places", value.span));
                };
                val.kind = kinds[0].clone();
                if !dropped(0) {
                    self.stays(&val, value.span)?;
                    val = self.keep(val, false);
                }
                vals.push((val, false));
                // The function kept a copy of each later text for whoever
                // receives it.
                for (slot, kind) in slots.iter().zip(&kinds[1..]) {
                    let ty = ir_type(kind, span)?;
                    let code = self.load(ty, slot);
                    vals.push((Val::new(code, kind.clone()).fixed(), true));
                }
            }
        }
        for ((place, (val, copied)), expected) in places.iter().zip(vals).zip(expected) {
            match place {
                crate::ast::Place::Name { name, .. } => {
                    let val = match &expected {
                        Some(expected) => self.coerce(val, expected, span)?,
                        None => val,
                    };
                    if is_empty_list(&val.kind) || is_empty_dict(&val.kind) {
                        return Err(cannot("an empty list or dict with no type beside it. Write `items: list[int]`", span));
                    }
                    let owner = self.types.owner_at(place) || (!self.collector && is_text(&val.kind));
                    self.declare(name, val, declare == Some(true), owner, span)?;
                }
                crate::ast::Place::Target(target) => self.store_in(target, val, span, span)?,
                // What nobody receives is given back: an owned value, and
                // with no collector a text the function kept a copy of.
                crate::ast::Place::Drop => {
                    if self.types.discarded_at(place) {
                        self.free(&val.kind, &val.code, span)?;
                    } else if !self.collector && is_text(&val.kind) && copied {
                        self.run("zeno_text_drop", format!("ptr {}", val.code));
                    }
                }
            }
        }
        Ok(())
    }

    /// A slot on the stack for the variable `name`. Two variables of one
    /// name in one function get two slots: `%var.total`, `%var.total.2`.
    fn stack_slot(&mut self, name: &str, ty: &str) -> String {
        let count = self.code.named.entry(name.to_string()).or_insert(0);
        *count += 1;
        let slot = match *count {
            1 => named('%', "var.", name),
            count => named('%', "var.", &format!("{name}.{count}")),
        };
        self.slot(slot, ty)
    }

    /// `x = v`, `point.x = v`, `items[i] = v`, and the same with `+=`.
    fn assign(&mut self, target: &Expr, op: Option<BinaryOp>, value: &Expr, span: Span) -> Result<(), Error> {
        // The new value first, then the place it goes to, as `zeno run`
        // does. `x += 1` is `x = x + 1`: what is there is read, and
        // then the value is worked out.
        let new = match op {
            None => self.expr(value)?,
            Some(op) => {
                let old = self.expr(target)?;
                let new = self.expr(value)?;
                self.arithmetic(op, old, new, span)?
            }
        };
        self.stays(&new, value.span)?;
        let new = self.keep(new, op.is_none() && is_written(value));
        self.store_in(target, new, value.span, span)
    }

    /// Put `new`, worked out and kept already, in the place `target`.
    /// `at` is where the value is written.
    fn store_in(&mut self, target: &Expr, new: Val, at: Span, span: Span) -> Result<(), Error> {
        // With no collector, a list that was just made (`xs += [4]` makes
        // one) has to arrive at an owner, which frees the list it had.
        let needs_owner = new.made && !self.collector;
        match &target.kind {
            ExprKind::Name(name) => {
                let Some(Variable { place: Place::Slot(slot), kind, owner, .. }) = self.lookup(name).cloned() else {
                    return Err(cannot(format!("changing `{name}`"), target.span));
                };
                if needs_owner && !owner {
                    return Err(no_owner(at));
                }
                let new = self.fit(new, &kind, span)?;
                let ty = ir_type(&kind, span)?;
                if owner {
                    // The new value was made first, since making it may
                    // read the old one. Now the old one is freed.
                    let old = self.load(ty, &slot);
                    self.free(&kind, &old, span)?;
                }
                self.store(ty, &new.code, &slot);
            }
            ExprKind::Field { object, name, .. } => {
                let object = self.expr(object)?;
                let field = match &object.kind {
                    Kind::Class(class) => self.field_of(class, name).map(|field| (class.clone(), field)),
                    _ => None,
                };
                let Some((class, (_, index, kind))) = field else {
                    return Err(cannot("an assignment to this", target.span));
                };
                let new = self.fit(new, &kind, span)?;
                let owned = self.classes[&class].owned_fields[index] || (!self.collector && is_text(&kind));
                if needs_owner && !owned {
                    return Err(no_owner(at));
                }
                if owned {
                    // As for an owner variable: the new value is made,
                    // and then the one the field held is freed.
                    let old = self.load_field(&class, &object.code, index, span)?;
                    self.free(&kind, &old, span)?;
                }
                self.store_field(&class, &object.code, index, &new.code, span)?;
            }
            ExprKind::Index { object, index } => {
                let object = self.expr(object)?;
                let index = self.expr(index)?;
                // Through a pointer: the item is stored at its own size,
                // and nothing is checked. That is what `unsafe` is for.
                if let Kind::Pointer(pointee) = object.kind {
                    let new = self.fit(new, &pointee.kind(), span)?;
                    let item = pointee.ir_type();
                    let place = self.set(format!("getelementptr {item}, ptr {}, i64 {}", object.code, index.code));
                    let value = match pointee {
                        Pointee::Sized(_) => self.set(format!("trunc i64 {} to {item}", new.code)),
                        _ => new.code.clone(),
                    };
                    self.ins(format!("store {item} {value}, ptr {place}"));
                    return Ok(());
                }
                // `ages["Ada"] = 36`. With no collector the value is
                // already a copy that the dict can call its own (`keep`,
                // above), and the dict frees the one it had.
                if let Kind::Dict(_, item) = &object.kind {
                    let new = self.fit(new, item, span)?;
                    let key = self.to_slot(&index, span)?;
                    let slot = self.to_slot(&new, span)?;
                    // A dict that owns its values frees the one that was
                    // under the key (decision 56.12).
                    if self.drops.owns(item) {
                        let put = self.dict_helper("put", item, span)?;
                        self.ins(format!("call void {put}(ptr {}, i64 {key}, i64 {slot})", object.code));
                        return Ok(());
                    }
                    self.run("zeno_dict_set", format!("ptr {}, i64 {key}, i64 {slot}", object.code));
                    return Ok(());
                }
                let Kind::List(item) = object.kind.clone() else {
                    return Err(cannot("an assignment to this", target.span));
                };
                // The list owns an item that has one owner. Any other
                // list has no use for one that was just made.
                let owns = self.drops.owns(&item);
                if needs_owner && !owns {
                    return Err(no_owner(at));
                }
                let new = self.fit(new, &item, span)?;
                let slot = self.kept_slot(&new, span)?;
                // The runtime checks the index and gives back the address
                // of that item's slot.
                let place = self.list_place(&object.code, &index.code, &item, span);
                if owns {
                    // The item that was there is freed, as a field's old
                    // value is, and the new one takes its place.
                    let old = self.set(format!("load ptr, ptr {place}"));
                    self.free(&item, &old, span)?;
                } else if !self.collector && is_text(&new.kind) {
                    // The list is done with the text it held there.
                    let old = self.set(format!("load ptr, ptr {place}"));
                    self.run("zeno_text_drop", format!("ptr {old}"));
                } else if !self.collector && is_boxed(&new.kind) {
                    // And with the box it kept the number in (56.14).
                    let old = self.set(format!("load ptr, ptr {place}"));
                    self.run("zeno_free", format!("ptr {old}"));
                }
                self.store_item(&slot, &place, &item);
            }
            _ => return Err(cannot("an assignment to this", target.span)),
        }
        Ok(())
    }

    /// Where a field of a class is: the class's struct type, the field's
    /// number, and its kind.
    fn field_of(&self, class: &str, name: &str) -> Option<(String, usize, Kind)> {
        let entry = self.classes.get(class)?;
        let index = entry.fields.iter().position(|(field, _, _)| field == name)?;
        Some((entry.ty.clone(), index, entry.fields[index].1.clone()))
    }

    // ------------------------------------------- With no collector to help
    //
    // `zeno build --llvm --no-collector` (docs/no-collector.md). Three
    // things need care that a collector would otherwise take: a value
    // that belongs to nobody for a moment (`hold`, further down), text
    // built while the program runs, and values that must not be kept.
    // What such a build refuses is refused where each kind of value is
    // made, in the C backend's words: search for `without_collector`.

    /// A statement is about to be written. In a build with no collector,
    /// text that the statement builds is written into the text room (see
    /// runtime.c) and stays there until the statement is over. Call
    /// `text_end` with what this returns when it is.
    fn text_begin(&mut self) -> (usize, bool) {
        self.open();
        (self.code.lines.len(), std::mem::replace(&mut self.code.built, false))
    }

    /// The statement begun at `text_begin` has been written. If it built
    /// text, note where the text room's top was before it:
    ///
    /// ```text
    ///   %mark.1 = load i64, ptr @zeno_text_top
    /// ```
    ///
    /// and return that name, for `text_back` to put the top back there
    /// once nothing needs the text any more. The line is written where
    /// the statement began, which is known only now: most statements
    /// build no text, and get no such line.
    fn text_end(&mut self, (start, outer): (usize, bool)) -> Option<String> {
        if !std::mem::replace(&mut self.code.built, outer) {
            return None;
        }
        self.code.marks += 1;
        let mark = format!("%mark.{}", self.code.marks);
        self.code.lines.insert(start, format!("  {mark} = load i64, ptr @zeno_text_top"));
        Some(mark)
    }

    /// Give back the text built since `mark` was noted. That is one
    /// `store`: there is nothing to search and nothing to fragment.
    fn text_back(&mut self, mark: &str) {
        self.ins(format!("store i64 {mark}, ptr @zeno_text_top"));
    }

    // ------------------------------------------------------------ Kept text
    //
    // In a build with no collector, text is kept by copy (docs/text.md).
    // Whatever stores text (a variable, a field, a list) has a copy of
    // its own in the pool and frees it when it is done. Text is never
    // changed once made, so a copy cannot be told from the original, and
    // the program means what it means in every other build.

    /// `val` is about to be stored. If it is text, store a copy that the
    /// place can call its own. `written` says the text is written in the
    /// program, which needs no copy.
    ///
    /// ```text
    ///   %t4 = call ptr @zeno_text_keep(ptr %t3)
    /// ```
    fn keep(&mut self, mut val: Val, written: bool) -> Val {
        if self.collector || !is_text(&val.kind) || written {
            return val;
        }
        let mut from = val.code.clone();
        if let Some(place) = val.lent.take() {
            // Text that was lent a moment ago, with nothing done since,
            // is copied straight from where it is kept: the statement's
            // own copy is not needed, and is not made.
            let lend = format!("  {} = call ptr @zeno_text_lend(ptr {place})", val.code);
            if self.code.lines.last() == Some(&lend) {
                self.code.lines.pop();
                from = place;
            }
        }
        val.code = self.call("ptr", "zeno_text_keep", format!("ptr {from}"));
        val
    }

    /// `val` was read from a place that a function could change while
    /// this statement is still using what it read: a variable at the top
    /// of the file, a field, an item of a list. If it is text, the
    /// statement gets a copy of its own, in the text room.
    fn lend(&mut self, val: Val) -> Val {
        if self.collector || !is_text(&val.kind) {
            return val.fixed();
        }
        let code = self.call("ptr", "zeno_text_lend", format!("ptr {}", val.code));
        Val { code, kind: val.kind, fixed: false, lent: Some(val.code), made: false, passing: false }
    }

    /// `val` is about to be kept: put in a variable, a field or a list,
    /// given to a function, or returned. In a build with no collector a
    /// value that the statement owns cannot be (see `belongs`). (Text
    /// can: see `keep`. An error can: whoever keeps it owns it.)
    /// Several values written out are all worked out before any reaches
    /// its place. In a firmware build a later one may fail, and the
    /// failure leaves at once. Hold each value that has to be freed until
    /// then, as a call holds an argument (decision 57). `copied` says a
    /// text is a copy of its own.
    fn hold_for_later(&mut self, val: &Val, copied: bool, later: &[Expr]) -> Option<String> {
        let frees = self.drops.owns(&val.kind) || (copied && is_text(&val.kind));
        let later_work = later.iter().any(|value| !emit_c::is_plain(value));
        (self.firmware && !self.collector && frees && later_work).then(|| self.hold(val.kind.clone(), &val.code))
    }

    fn stays(&self, val: &Val, span: Span) -> Result<(), Error> {
        if !self.collector && val.passing {
            // It is freed when this statement is over.
            return Err(no_owner(span));
        }
        Ok(())
    }

    // ------------------------------------------------ What a statement owns
    //
    // A value that is made in the middle of a statement and given to
    // nobody belongs to the statement (docs/temporaries.md):
    //
    //     print(len(text.split()))      the list of words
    //     print(size(make()))           the owned value `make` gives
    //
    // The statement keeps its address in a slot and frees it when it is
    // over:
    //
    //   store ptr null, ptr %own.1               ; where the statement begins
    //   %t1 = call ptr @zeno_split_blanks(ptr %t0)
    //   store ptr %t1, ptr %own.1
    //   ...
    //   %t5 = load ptr, ptr %own.1               ; where it ends
    //   call void @zeno_list_drop_text(ptr %t5)
    //
    // A slot, and not simply `%t1`, because the value may be made in a
    // block that `and` or `or` skips, and a name from such a block may
    // not be used after it. The slot holds null until the value is made,
    // and freeing null does nothing.

    /// A statement begins, or a part of one that is over before the
    /// statement is: a condition, one item of a comprehension.
    fn own_begin(&mut self) -> (usize, usize) {
        self.open();
        (self.code.lines.len(), self.code.passing.len())
    }

    /// It is over. Write the lines that empty the slots, where it began.
    /// If `free`, free the values here. A statement that leaves
    /// (`return`, `raise`) has freed them on its way out, as it frees
    /// what the owners hold, and passes `false`.
    fn own_end(&mut self, (start, from): (usize, usize), free: bool) -> Result<(), Error> {
        let ended: Vec<Passing> = self.code.passing.drain(from..).collect();
        for (position, passing) in ended.iter().enumerate() {
            let how = self.how(&passing.slot);
            self.code.lines.insert(start + position, format!("  store{how} ptr null, ptr {}", passing.slot));
        }
        // The last one made may be looking at an earlier one.
        for passing in ended.iter().rev() {
            if free {
                let value = self.load("ptr", &passing.slot);
                self.free(&passing.kind, &value, Span { line: 0, col: 0 })?;
            }
            self.let_go(&passing.key);
        }
        Ok(())
    }

    /// `val` is what `expr` gave. If it was made right here and nobody is
    /// getting it, the statement owns it from now on.
    ///
    /// Two kinds of value are in that position. An owned value that is
    /// only looked at, which the checker has noted (`Types::lent`), in
    /// every build. And, in a build with no collector, a list that was
    /// just made and does not start life owned: every other build leaves
    /// such a list to the collector.
    fn belongs(&mut self, mut val: Val, expr: &Expr) -> Val {
        let lent = self.types.lent(expr);
        if self.walked == Some(expr as *const Expr as usize) {
            // The `for` loop that walks it is the one that frees it.
            val.made |= lent;
            return val;
        }
        let nobodys = val.made && !self.collector && !self.types.fresh(expr);
        if !lent && !nobodys {
            return val;
        }
        self.code.owns += 1;
        let slot = self.slot(format!("%own.{}", self.code.owns), "ptr");
        self.store("ptr", &val.code, &slot);
        // Held in its slot, as an owner is: leaving early frees it.
        self.code.holds += 1;
        let key = format!("held {}", self.code.holds);
        let variable = Variable { place: Place::Slot(slot.clone()), kind: val.kind.clone(), open: false, owner: true, inner: None };
        self.scopes.last_mut().expect("a scope").insert(key.clone(), variable);
        self.code.passing.push(Passing { key, slot, kind: val.kind.clone() });
        val.made = false;
        val.passing = true;
        val
    }

    /// Each item of a list sits in one slot. A number that may be `none`
    /// does not fit, and is kept in a small block of its own, which
    /// nothing would free.
    /// Does a list of these keep a copy of its own of each item? With no
    /// collector it does of text, and of the box of each number that may
    /// be `none` (decision 56.14).
    fn keeps_copies(&self, item: &Kind) -> bool {
        !self.collector && (is_text(item) || is_boxed(item))
    }

    /// A list just made whose items are still another's (`+`, `*`, a
    /// slice, the copy a loop walks) gets copies of its own.
    fn keep_items(&mut self, item: &Kind, list: &str, span: Span) -> Result<String, Error> {
        Ok(match item {
            item if is_text(item) => self.call("ptr", "zeno_list_keep_text", format!("ptr {list}")),
            Kind::Optional(inner) => {
                let keep = box_function("list.keep.boxes", scalar(inner, span)?);
                self.call("ptr", keep, format!("ptr {list}"))
            }
            _ => list.to_string(),
        })
    }

    /// A value on its way into a list that keeps it. With no collector a
    /// number that may be `none` goes into a box of the list's own, in
    /// the pool, which the list frees (decision 56.14).
    fn kept_slot(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        match &val.kind {
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => {
                let keep = box_function("box.keep", scalar(inner, span)?);
                let boxed = self.call("ptr", keep, format!("{} {}", ir_type(&val.kind, span)?, val.code));
                Ok(self.set(format!("ptrtoint ptr {boxed} to i64")))
            }
            _ => self.to_slot(val, span),
        }
    }

    /// A value that is only looked for (`find`, `in`). A number that may
    /// be `none` goes into a box in the text room, which, with no
    /// collector, is given back when the statement is over.
    fn looked_for(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        if !self.collector && is_boxed(&val.kind) {
            self.code.built = true;
        }
        self.to_slot(val, span)
    }

    /// An item leaving its list for good (`pop`, `take`), from `slot`:
    /// the list's copy of text is freed once the statement has its own,
    /// and the list's box once the number is out.
    fn taken(&mut self, item: &Kind, slot: &str, span: Span) -> Result<Val, Error> {
        match item {
            item if !self.collector && is_text(item) => {
                let val = self.from_slot(item, slot, span)?;
                Ok(Val::new(self.call("ptr", "zeno_text_take", format!("ptr {}", val.code)), val.kind))
            }
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => {
                let take = box_function("box.take", scalar(inner, span)?);
                let boxed = self.set(format!("inttoptr i64 {slot} to ptr"));
                Ok(Val::new(self.call(ir_type(item, span)?, take, format!("ptr {boxed}")), item.clone()))
            }
            item => self.from_slot(item, slot, span),
        }
    }

    fn fits_a_slot(&self, item: &Kind, span: Span) -> Result<(), Error> {
        match item {
            Kind::Optional(inner) if !self.collector && !is_pointer(inner) => Err(emit_c::none_in_a_slot(span)),
            _ => Ok(()),
        }
    }

    // --------------------------------------------------------- Owned values
    //
    // An owned value is freed by its owner, at the end of the owner's
    // block (docs/ownership.md). In the IR an owner is a slot that holds
    // an address, with one extra rule: when its value is given away, null
    // is stored in the slot. So "free whatever the owners still hold" is
    // one load and one call for each owner, and the call does nothing for
    // null. The checker has already made sure a variable is never read
    // after it gave its value away.

    /// The first block that belongs to the function being written. The
    /// one before it, in a function, is the top of the file.
    fn base(&self) -> usize {
        self.code.in_function as usize
    }

    /// Free the owned value at `value`, a list or an object, if there is
    /// one there.
    fn free(&mut self, kind: &Kind, value: &str, span: Span) -> Result<(), Error> {
        match kind {
            Kind::Class(class) => {
                let drop = self.drop_class(class, span)?;
                self.ins(format!("call void {drop}(ptr {value})"));
            }
            Kind::Enum(owner) => {
                let drop = self.drop_enum(owner, span)?;
                self.ins(format!("call void {drop}(ptr {value})"));
            }
            // What may be `none` is a pointer that may be null, and every
            // function that frees does nothing with null.
            Kind::Optional(inner) if is_pointer(inner) => return self.free(inner, value, span),
            // An error that an `except` caught, in a build with no collector.
            Kind::Error => self.run("zeno_error_drop", format!("ptr {value}")),
            // A function that was written where a value goes frees the
            // block it is kept in. A function of the program has none,
            // and is left alone.
            Kind::Function(_) => self.run("zeno_closure_drop", format!("ptr {value}")),
            // Text that is kept by copy, and a list of such text, which
            // frees its items as well as itself.
            kind if is_text(kind) => self.run("zeno_text_drop", format!("ptr {value}")),
            // A dict frees its two lists, its index and, with no
            // collector, its copies of any text.
            // One that owns its values frees each first (decision 56.12).
            Kind::Dict(_, item) if self.drops.owns(item) => {
                let drop = self.dict_helper("drop", item, span)?;
                self.ins(format!("call void {drop}(ptr {value})"));
            }
            Kind::Dict(..) => self.run("zeno_dict_drop", format!("ptr {value}")),
            Kind::List(item) if !self.collector && is_text(item) => {
                self.run("zeno_list_drop_text", format!("ptr {value}"))
            }
            // Numbers that may be `none`, each in a box the list owns (56.14).
            Kind::List(item) if !self.collector && is_boxed(item) => self.run("zeno.list.drop.boxes", format!("ptr {value}")),
            // A list of items that have one owner each, the list: its
            // end is theirs.
            Kind::List(item) if self.drops.owns(item) => {
                let drop = self.drop_list(item, span)?;
                self.ins(format!("call void {drop}(ptr {value})"));
            }
            _ => self.run("zeno_list_drop", format!("ptr {value}")),
        }
        Ok(())
    }

    /// The function that ends a list together with its items, for a
    /// list whose items have one owner each. Null means the list was
    /// given away, and there is nothing to do.
    ///
    /// ```text
    /// define internal void @zeno_list_drop_1_Point(ptr %list) {
    /// entry:
    ///   %t1 = icmp eq ptr %list, null
    ///   br i1 %t1, label %gone1, label %there1
    /// gone1:
    ///   ret void
    /// there1:
    ///   ...for each item:
    ///   call void @zeno.drop.Point(ptr %t9)
    ///   ...then the list itself:
    ///   call void @zeno_list_drop(ptr %list)
    ///   ret void
    /// }
    /// ```
    fn drop_list(&mut self, item: &Kind, span: Span) -> Result<String, Error> {
        // The C backend's name for it. No class can be given a name
        // that makes the same one.
        let name = format!("@{}", self.drops.name(&Kind::List(Box::new(item.clone()))));
        // Noted as written before it is, as for a class.
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let item = item.clone();
        self.side(format!("define internal void {name}(ptr %list)"), |emitter| {
            let gone = emitter.set("icmp eq ptr %list, null");
            let n = emitter.number();
            let (nothing, there) = (format!("gone{n}"), format!("there{n}"));
            emitter.jump_if(&gone, &nothing, &there);
            emitter.place(&nothing);
            emitter.end("ret void");
            emitter.place(&there);
            let (count, slot) = emitter.walk_open("item", "%list", &item);
            let each = emitter.from_slot(&item, &slot, span)?;
            emitter.free(&item, &each.code, span)?;
            emitter.count_close(&count);
            emitter.run("zeno_list_drop", "ptr %list");
            emitter.end("ret void");
            Ok(())
        })?;
        Ok(name)
    }

    /// `dict_kinds`, for a dict this program makes. With a collector a
    /// dict is shared, and cannot own its values (see the C backend's
    /// `dict_letters`).
    fn dict_kinds(&self, key: &Kind, value: &Kind, span: Span) -> Result<String, Error> {
        if self.collector && self.drops.owns(value) {
            return Err(emit_c::owned_in_shared_dict("`zeno build --llvm`", span));
        }
        dict_kinds(key, value, span)
    }

    /// A function for a dict that owns its values of kind `value`, by the
    /// C backend's name (decision 56.12): `drop` frees the dict and each
    /// value, `clear` frees each value and empties the dict, and `put`
    /// stores a value under a key and frees the one that was there.
    ///
    /// ```text
    /// define internal void @zeno_dict_put_values_Frame(ptr %dict, i64 %key, i64 %value) {
    /// entry:
    ///   %t1 = call i64 @zeno_dict_find(ptr %dict, i64 %key)
    ///   %t2 = call i64 @zeno_dict_value(ptr %dict, i64 %t1)
    ///   %t3 = inttoptr i64 %t2 to ptr
    ///   call void @zeno.drop.Frame(ptr %t3)
    ///   call void @zeno_dict_set(ptr %dict, i64 %key, i64 %value)
    ///   ret void
    /// }
    /// ```
    fn dict_helper(&mut self, what: &str, value: &Kind, span: Span) -> Result<String, Error> {
        let name = format!("@{}", self.drops.dict_helper(what, value));
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let value = value.clone();
        match what {
            "drop" => self.side(format!("define internal void {name}(ptr %dict)"), |emitter| {
                let gone = emitter.set("icmp eq ptr %dict, null");
                let n = emitter.number();
                let (nothing, there) = (format!("gone{n}"), format!("there{n}"));
                emitter.jump_if(&gone, &nothing, &there);
                emitter.place(&nothing);
                emitter.end("ret void");
                emitter.place(&there);
                let clear = emitter.dict_helper("clear", &value, span)?;
                emitter.ins(format!("call void {clear}(ptr %dict)"));
                emitter.run("zeno_dict_drop", "ptr %dict");
                emitter.end("ret void");
                Ok(())
            })?,
            "clear" => self.side(format!("define internal void {name}(ptr %dict)"), |emitter| {
                // A dict starts with its list of keys, then its values.
                let at = emitter.field_at("{ ptr, ptr }", "%dict", 1);
                let values = emitter.set(format!("load ptr, ptr {at}"));
                let (count, slot) = emitter.walk_open("item", &values, &value);
                let each = emitter.from_slot(&value, &slot, span)?;
                emitter.free(&value, &each.code, span)?;
                emitter.count_close(&count);
                emitter.run("zeno_dict_clear", "ptr %dict");
                emitter.end("ret void");
                Ok(())
            })?,
            _ => self.side(format!("define internal void {name}(ptr %dict, i64 %key, i64 %value)"), |emitter| {
                let at = emitter.call("i64", "zeno_dict_find", "ptr %dict, i64 %key".to_string());
                let old = emitter.call("i64", "zeno_dict_value", format!("ptr %dict, i64 {at}"));
                let each = emitter.from_slot(&value, &old, span)?;
                emitter.free(&value, &each.code, span)?;
                emitter.run("zeno_dict_set", "ptr %dict, i64 %key, i64 %value");
                emitter.end("ret void");
                Ok(())
            })?,
        }
        Ok(name)
    }

    /// Free what the owners in `scopes[from..]` still hold, innermost
    /// block first: a block is ending, or being left early.
    ///
    /// ```text
    ///   %t9 = load ptr, ptr %var.frame
    ///   call void @zeno.drop.Frame(ptr %t9)
    /// ```
    ///
    /// The resources of a block are released, last made first, before
    /// its owners are freed (decision 59.1).
    fn free_owned(&mut self, from: usize) -> Result<(), Error> {
        if self.code.resources.iter().any(|(at, _)| *at >= from) {
            for depth in (from..self.scopes.len()).rev() {
                let releases: Vec<String> =
                    self.code.resources.iter().rev().filter(|(at, _)| *at == depth).map(|(_, name)| name.clone()).collect();
                for name in releases {
                    self.release(depth, &name, Span { line: 0, col: 0 })?;
                }
                self.free_block(depth)?;
            }
            return Ok(());
        }
        let mut owners = Vec::new();
        for scope in self.scopes.iter().skip(from).rev() {
            let mut here: Vec<(String, bool, Kind)> = scope
                .values()
                .filter(|variable| variable.owner)
                .map(|variable| match &variable.place {
                    Place::Slot(slot) => (slot.clone(), true, variable.kind.clone()),
                    Place::Value(code) => (code.clone(), false, variable.kind.clone()),
                })
                .collect();
            // In the same order on every run of the compiler.
            here.sort_by(|a, b| a.0.cmp(&b.0));
            owners.extend(here);
        }
        for (place, is_slot, kind) in owners {
            let value = if is_slot { self.load("ptr", &place) } else { place };
            self.free(&kind, &value, Span { line: 0, col: 0 })?;
        }
        Ok(())
    }

    /// Free what the owners of the one block `scopes[depth]` still hold.
    fn free_block(&mut self, depth: usize) -> Result<(), Error> {
        let mut owners: Vec<(String, bool, Kind)> = self.scopes[depth]
            .values()
            .filter(|variable| variable.owner)
            .map(|variable| match &variable.place {
                Place::Slot(slot) => (slot.clone(), true, variable.kind.clone()),
                Place::Value(code) => (code.clone(), false, variable.kind.clone()),
            })
            .collect();
        owners.sort_by(|a, b| a.0.cmp(&b.0));
        for (place, is_slot, kind) in owners {
            let value = if is_slot { self.load("ptr", &place) } else { place };
            self.free(&kind, &value, Span { line: 0, col: 0 })?;
        }
        Ok(())
    }

    /// `value` is an owned value that has no owner just now: it is on its
    /// way into a call, or it is a list still being filled, or the list a
    /// `for` loop walks. Until `let_go`, whatever leaves the block early
    /// (a `return`, a failed call) frees it, as it frees what the block's
    /// owners hold.
    fn hold(&mut self, kind: Kind, value: &str) -> String {
        self.code.holds += 1;
        // No Zeno name has a space in it, so this cannot hide a variable.
        let key = format!("held {}", self.code.holds);
        let variable = Variable { place: Place::Value(value.to_string()), kind, open: false, owner: true, inner: None };
        self.scopes.last_mut().expect("a scope").insert(key.clone(), variable);
        key
    }

    /// The value has arrived where it was going, and has an owner again.
    fn let_go(&mut self, key: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.remove(key).is_some() {
                return;
            }
        }
    }

    /// The function that ends an object of an owned class: free what its
    /// fields own, then the object. Null means the object was given
    /// away, and there is nothing to do.
    ///
    /// ```text
    /// define internal void @zeno.drop.Frame(ptr %self) {
    /// entry:
    ///   %t1 = icmp eq ptr %self, null
    ///   br i1 %t1, label %gone1, label %there1
    /// gone1:
    ///   ret void
    /// there1:
    ///   %t2 = getelementptr %z_Frame, ptr %self, i32 0, i32 1
    ///   %t3 = load ptr, ptr %t2
    ///   call void @zeno_list_drop(ptr %t3)
    ///   call void @zeno_free(ptr %self)
    ///   ret void
    /// }
    /// ```
    fn drop_class(&mut self, class: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.drop.", class);
        // Noted as written before it is: an object may own another of its
        // own class, and freeing that one calls this same function.
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        if self.classes[class].interface {
            // An object seen as a `Shape` is freed as what it is.
            let classes = self.is_a.get(class).cloned().unwrap_or_default();
            let mut drops = Vec::new();
            for (_, class) in &classes {
                drops.push(self.drop_class(class, span)?);
            }
            self.side(format!("define internal void {name}(ptr %self)"), |emitter| {
                let gone = emitter.set("icmp eq ptr %self, null");
                emitter.jump_if(&gone, "gone", "there");
                emitter.place("gone");
                emitter.end("ret void");
                emitter.place("there");
                let blocks = emitter.switch_on_class("%self", &classes);
                for (block, drop) in blocks.iter().zip(&drops) {
                    emitter.place(block);
                    emitter.ins(format!("call void {drop}(ptr %self)"));
                    emitter.end("ret void");
                }
                Ok(())
            })?;
            return Ok(name);
        }
        // With no collector an object has its own copies of the text in
        // its fields, and those end with it.
        let texts = !self.collector;
        let entry = &self.classes[class];
        let owned: Vec<(usize, Kind)> = entry
            .fields
            .iter()
            .enumerate()
            .filter(|(index, (_, kind, _))| entry.owned_fields[*index] || (texts && is_text(kind)))
            .map(|(index, (_, kind, _))| (index, kind.clone()))
            .collect();
        self.side(format!("define internal void {name}(ptr %self)"), |emitter| {
            let gone = emitter.set("icmp eq ptr %self, null");
            let n = emitter.number();
            let (nothing, there) = (format!("gone{n}"), format!("there{n}"));
            emitter.jump_if(&gone, &nothing, &there);
            emitter.place(&nothing);
            emitter.end("ret void");
            emitter.place(&there);
            for (index, kind) in &owned {
                let held = emitter.load_field(class, "%self", *index, span)?;
                emitter.free(kind, &held, span)?;
            }
            emitter.run("zeno_free", "ptr %self");
            emitter.end("ret void");
            Ok(())
        })?;
        Ok(name)
    }

    /// The function that ends a value of an enum whose values have one
    /// owner each. A case that carries nothing is one fixed value and is
    /// never freed. A case that carries something is a block: free what
    /// it carries that has an owner, and the text it keeps, then the
    /// block.
    fn drop_enum(&mut self, owner: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.drop.", owner);
        // Noted as written before it is: a value may carry another of
        // its own enum.
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let texts = !self.collector;
        let cases = self.enums[owner].cases.clone();
        let takes = self.enums[owner].takes.clone();
        self.side(format!("define internal void {name}(ptr %self)"), |emitter| {
            let gone = emitter.set("icmp eq ptr %self, null");
            let n = emitter.number();
            let (nothing, there) = (format!("gone{n}"), format!("there{n}"));
            emitter.jump_if(&gone, &nothing, &there);
            emitter.place(&nothing);
            emitter.end("ret void");
            emitter.place(&there);
            let tag = emitter.set("load i64, ptr %self");
            let ways: Vec<String> = cases
                .iter()
                .enumerate()
                .filter(|(_, (_, fields))| !fields.is_empty())
                .map(|(index, _)| format!("i64 {index}, label %case{index}"))
                .collect();
            emitter.end(format!("switch i64 {tag}, label %bare [ {} ]", ways.join(" ")));
            for (index, (case, fields)) in cases.iter().enumerate() {
                if fields.is_empty() {
                    continue;
                }
                emitter.place(&format!("case{index}"));
                let ty = case_type(owner, case);
                for (position, (_, kind)) in fields.iter().enumerate() {
                    if takes[index][position] || (texts && is_text(kind)) {
                        let at = emitter.field_at(&ty, "%self", position + 1);
                        let held = emitter.set(format!("load {}, ptr {at}", ir_type(kind, span)?));
                        emitter.free(kind, &held, span)?;
                    }
                }
                emitter.run("zeno_free", "ptr %self");
                emitter.end("ret void");
            }
            emitter.place("bare");
            emitter.end("ret void");
            Ok(())
        })?;
        Ok(name)
    }

    // ---------------------------------------------------------------- Loops

    /// Begin a loop that counts up by one from `start`. The counter
    /// changes, so it has a slot, like a `var`. Returns the loop, and the
    /// counter as it stands at the top of this trip. The caller decides
    /// whether there is another trip, and says so with `count_body`.
    fn count_open(&mut self, variable: &str, start: &str) -> (Count, String) {
        let n = self.number();
        let counter = self.slot(named('%', "for.", &format!("{variable}.{n}")), "i64");
        self.store("i64", start, &counter);
        let count = Count {
            counter,
            test: format!("for{n}"),
            body: format!("do{n}"),
            next: format!("next{n}"),
            done: format!("done{n}"),
            walked: None,
            values: None,
            held: None,
        };
        self.place(&count.test);
        let trip = self.load("i64", &count.counter);
        (count, trip)
    }

    /// The test is written: go round again if `more` holds.
    fn count_body(&mut self, count: &Count, more: &str) {
        self.jump_if(more, &count.body, &count.done);
        self.place(&count.body);
    }

    /// The end of the loop's body. `continue` comes here: add one and
    /// test again. The counter is below its limit, so adding one cannot
    /// overflow.
    fn count_close(&mut self, count: &Count) {
        self.place(&count.next);
        let trip = self.load("i64", &count.counter);
        let up = self.set(format!("add i64 {trip}, 1"));
        self.store("i64", &up, &count.counter);
        self.jump(&count.test);
        self.place(&count.done);
    }

    /// Begin a loop over the items of the list at `items`. Returns the
    /// loop, and the slot of this trip's item.
    fn walk_open(&mut self, variable: &str, items: &str, item: &Kind) -> (Count, String) {
        let (count, trip) = self.count_open(variable, "0");
        // A list starts with how many items it has.
        let len = self.set(format!("load i64, ptr {items}"));
        let more = self.set(format!("icmp slt i64 {trip}, {len}"));
        self.count_body(&count, &more);
        // Its third field is where the items are, one slot after another.
        // This `getelementptr` has no `i32 0` in front: it steps along an
        // array of slots, where the others stepped into a struct.
        let slot = self.slot_of(items, &trip, item);
        (count, slot)
    }

    /// The address of item `index` of the list at `list`. The runtime
    /// checks the index; with `--fast` it is worked out here, unchecked.
    fn list_place(&mut self, list: &str, index: &str, item: &Kind, span: Span) -> String {
        let (step, checked) = match item {
            Kind::Byte => ("i8", "zeno_bytes_at"),
            _ => ("i64", "zeno_list_at"),
        };
        if self.fast {
            let array_at = self.field_at("%zeno_list", list, 2);
            let array = self.set(format!("load ptr, ptr {array_at}"));
            return self.set(format!("getelementptr {step}, ptr {array}, i64 {index}"));
        }
        self.call("ptr", checked, format!("ptr {list}, i64 {index}, {}", at(span)))
    }

    /// The slot at `place`, as an i64: a byte is read and made wide.
    fn load_item(&mut self, place: &str, item: &Kind) -> String {
        match item {
            Kind::Byte => {
                let byte = self.set(format!("load i8, ptr {place}"));
                self.set(format!("zext i8 {byte} to i64"))
            }
            _ => self.set(format!("load i64, ptr {place}")),
        }
    }

    /// Put `slot` at `place`: a byte is cut down to its eight bits.
    fn store_item(&mut self, slot: &str, place: &str, item: &Kind) {
        match item {
            Kind::Byte => {
                let byte = self.set(format!("trunc i64 {slot} to i8"));
                self.ins(format!("store i8 {byte}, ptr {place}"));
            }
            _ => self.ins(format!("store i64 {slot}, ptr {place}")),
        }
    }

    /// The slot at position `at` of the list at `items`.
    fn slot_of(&mut self, items: &str, at: &str, item: &Kind) -> String {
        let array_at = self.field_at("%zeno_list", items, 2);
        let array = self.set(format!("load ptr, ptr {array_at}"));
        let step = if *item == Kind::Byte { "i8" } else { "i64" };
        let slot_at = self.set(format!("getelementptr {step}, ptr {array}, i64 {at}"));
        self.load_item(&slot_at, item)
    }

    /// The first part of a `for` loop: `for i in range(...)` or `for x in
    /// items`. It declares the loop variable in a scope of its own. The
    /// caller writes the body and then calls `close_loop`.
    ///
    /// `begin` is for a `for` statement in a build with no collector:
    /// text built to find what to loop over is given back once the loop
    /// has its list or its numbers in hand, before the first trip.
    ///
    /// With two names, `for key, value in ages`, `second` is the second:
    /// the first is then the key of a dict, or the position in a list, a
    /// str or a range, and the second is what is there.
    fn open_loop(
        &mut self,
        variable: &str,
        second: Option<&str>,
        iterable: &Expr,
        begin: Option<(usize, bool)>,
    ) -> Result<Count, Error> {
        let span = iterable.span;
        // What the first name is given, where there are two.
        let mut first: Option<Val> = None;
        // The list of a dict's values, for a loop over its pairs.
        let mut values: Option<(String, Kind)> = None;
        let mut value_kind = Kind::None;
        let range = match &iterable.kind {
            ExprKind::Call { callee, args }
                if matches!(&callee.kind, ExprKind::Name(name) if name == "range" && self.lookup(name).is_none())
                    && !self.functions.contains_key("range")
                    && args.iter().all(|arg| arg.name.is_none()) =>
            {
                Some(args)
            }
            _ => None,
        };
        let (mut count, value, walked) = match range {
            // A range becomes a plain counting loop: no list of numbers
            // is built.
            Some(bounds) => {
                // The ends are worked out once, before the loop.
                let mut ends = Vec::new();
                for bound in bounds {
                    let val = self.expr(&bound.value)?;
                    if val.kind != Kind::Int {
                        return Err(cannot("a `range` of anything but ints", span));
                    }
                    ends.push(val.code);
                }
                // How many trips, and how to get the loop's number from
                // the trip.
                let (start, stop, step) = match ends.as_slice() {
                    [stop] => ("0".to_string(), stop.clone(), None),
                    [start, stop] => (start.clone(), stop.clone(), None),
                    // With a step, count the trips first. Stepping up to
                    // `stop` directly could run past the end of what an
                    // int holds.
                    [start, _, step] => {
                        let args = format!("i64 {}, {}", ends.join(", i64 "), at(span));
                        let trips = self.call("i64", "zeno_range_count", args);
                        ("0".to_string(), trips, Some((start.clone(), step.clone())))
                    }
                    _ => return Err(cannot("this `range`", span)),
                };
                self.text_given_back(begin);
                let (count, trip) = self.count_open(variable, &start);
                let more = self.set(format!("icmp slt i64 {trip}, {stop}"));
                self.count_body(&count, &more);
                // With two names the first counts the trips from 0.
                if second.is_some() {
                    let trips = match (&step, start.as_str()) {
                        (None, "0") | (Some(_), _) => trip.clone(),
                        (None, start) => self.set(format!("sub i64 {trip}, {start}")),
                    };
                    first = Some(Val::new(trips, Kind::Int));
                }
                let number = match step {
                    None => trip,
                    Some((start, step)) => {
                        let along = self.set(format!("mul i64 {trip}, {step}"));
                        self.set(format!("add i64 {start}, {along}"))
                    }
                };
                (count, Val::new(number, Kind::Int), None)
            }
            // `for x in items`. The loop walks the list as it was when
            // the loop began, so a body that adds to the list or removes
            // from it cannot confuse the loop. That is what `zeno run`
            // does too.
            None => 'list: {
                self.walked = Some(iterable as *const Expr as usize);
                let list = self.expr(iterable);
                self.walked = None;
                let list = list?;
                // A loop over a dict walks a list of its keys, made for
                // the loop. A dict that was made right here (`for key in
                // ["a": 1]`) is nobody's, and has done its work once the
                // keys are out of it.
                let list = match &list.kind {
                    Kind::Dict(key, value) => {
                        let keys = self.call("ptr", "zeno_dict_keys", format!("ptr {}", list.code));
                        if second.is_some() {
                            let each = self.call("ptr", "zeno_dict_values", format!("ptr {}", list.code));
                            // The values still belong to the dict. With
                            // no collector the list has its own copies
                            // of the ones that are text.
                            // A dict made right here that owns its values
                            // gives them to the list (decision 56.12).
                            let whole = (!self.collector && is_text(value)) || (list.made && self.drops.owns(value));
                            let kind = Kind::List(Box::new(if whole { (**value).clone() } else { Kind::None }));
                            values = Some((each, kind));
                            value_kind = (**value).clone();
                        }
                        match (list.made, second.is_some()) {
                            (true, true) => self.run("zeno_dict_drop", format!("ptr {}", list.code)),
                            (true, false) => self.free(&list.kind.clone(), &list.code, span)?,
                            _ => {}
                        }
                        Val::new(keys, Kind::List(key.clone())).made()
                    }
                    _ => list,
                };
                // `for ch in text` walks the text where it is, and builds
                // no list of its characters (see the C backend's
                // `open_text_loop`). The counter is how many bytes in the
                // walk is; each trip moves it on by its character's size.
                let stays = match &iterable.kind {
                    ExprKind::Name(_) => self.text_stays,
                    ExprKind::Str(_) => true,
                    _ => false,
                };
                if list.kind == Kind::Str && second.is_none() && (self.collector || stays) {
                    self.text_given_back(begin);
                    let n = self.number();
                    let held = self.slot(named('%', "held.", &format!("{variable}.{n}")), "ptr");
                    self.store("ptr", "null", &held);
                    let (mut count, trip) = self.count_open(variable, "0");
                    let at = self.set(format!("getelementptr i8, ptr {}, i64 {trip}", list.code));
                    let byte = self.set(format!("load i8, ptr {at}"));
                    let more = self.set(format!("icmp ne i8 {byte}, 0"));
                    self.count_body(&count, &more);
                    // `count_close` adds one more.
                    let size = self.call("i64", "zeno_char_size", format!("ptr {at}"));
                    let after = self.set(format!("add i64 {trip}, {size}"));
                    let last = self.set(format!("sub i64 {after}, 1"));
                    self.store("i64", &last, &count.counter);
                    let ch = self.call("ptr", "zeno_char_here", format!("ptr {at}, ptr {held}"));
                    if !self.collector {
                        count.held = Some(held);
                    }
                    break 'list (count, Val::new(ch, Kind::Str).fixed(), None);
                }
                // A list made right here is nobody else's, so it needs no
                // copy: one written out, or what `split`, `map`, `filter`
                // or `+` gave, or an owned list that a function gave back.
                let fresh = list.made;
                // Nor does a list that the statement owns: nothing the
                // body does can reach it. The statement frees it, after
                // the loop.
                // Nor does a list the body cannot change (see the C
                // backend's `open_list_loop`).
                let own = (list.passing || (self.in_place && !fresh)) && list.kind != Kind::Str;
                let (items, item) = match &list.kind {
                    Kind::List(item) if fresh || own => (list.code.clone(), (**item).clone()),
                    // With no collector a list of text has its own copies
                    // of its items, and so does the copy the loop walks:
                    // the body may give the list new items while the loop
                    // looks at the old.
                    Kind::List(item) if !self.collector && is_text(item) => {
                        (self.call("ptr", "zeno_list_copy_text", format!("ptr {}", list.code)), (**item).clone())
                    }
                    // The same for the boxes of numbers that may be `none`
                    // (56.14).
                    Kind::List(item) if self.keeps_copies(item) => {
                        let copy = self.call("ptr", list_fn(item, "copy"), format!("ptr {}", list.code));
                        (self.keep_items(item, &copy, span)?, (**item).clone())
                    }
                    Kind::List(item) => {
                        (self.call("ptr", list_fn(item, "copy"), format!("ptr {}", list.code)), (**item).clone())
                    }
                    // A str is looped over one character at a time: the
                    // loop walks a list of them, which is its own.
                    Kind::Str => (self.call("ptr", "zeno_chars", format!("ptr {}", list.code)), Kind::Str),
                    _ => return Err(cannot("a `for` loop over anything but a list, a str or `range(...)`", span)),
                };
                self.text_given_back(begin);
                let (count, slot) = self.walk_open(variable, &items, &item);
                if second.is_some() && values.is_none() {
                    // Over a list the first name is the position.
                    let trip = self.load("i64", &count.counter);
                    first = Some(Val::new(trip, Kind::Int));
                }
                // What frees the list: its items too, if they are text
                // it has copies of, or if the list is the one that was
                // made and its items have no other owner. The items in
                // a copy still belong to the list that was copied.
                let whole = self.keeps_copies(&item) || (fresh && self.drops.owns(&item));
                let kind = Kind::List(Box::new(if whole { item.clone() } else { Kind::None }));
                (count, self.from_slot(&item, &slot, span)?.fixed(), (!own).then_some((items, kind)))
            }
        };
        // Over a dict the first name is the key, which is what the list
        // of keys gave, and the second is in the list of values at the
        // same position.
        let (value, first) = match (&values, first) {
            (Some((list, _)), _) => {
                let trip = self.load("i64", &count.counter);
                let slot = self.slot_of(list, &trip, &value_kind);
                (self.from_slot(&value_kind, &slot, span)?.fixed(), Some(value))
            }
            (None, first) => (value, first),
        };
        // What the body sees is a value, which it cannot change.
        let seen = |val: Val| Variable { place: Place::Value(val.code), kind: val.kind, open: false, owner: false, inner: None };
        let mut scope = HashMap::new();
        match (second, first) {
            (Some(second), Some(first)) => {
                scope.insert(variable.to_string(), seen(first));
                scope.insert(second.to_string(), seen(value));
            }
            _ => {
                scope.insert(variable.to_string(), seen(value));
            }
        }
        // The last character that was not ASCII is the loop's: leaving
        // the loop early frees it.
        if let Some(held) = &count.held {
            self.code.holds += 1;
            let variable = Variable { place: Place::Slot(held.clone()), kind: Kind::Str, open: false, owner: true, inner: None };
            scope.insert(format!("held {}", self.code.holds), variable);
        }
        self.scopes.push(scope);
        if let Some((list, kind)) = &values {
            self.hold(kind.clone(), list);
        }
        count.values = values;
        // The list being walked is the loop's own: a copy, or a list
        // written out for it. A `return` or a failed call inside the loop
        // frees it on the way out; `close_loop` does when the loop ends
        // by itself.
        if let Some((items, kind)) = &walked {
            self.hold(kind.clone(), items);
        }
        count.walked = walked;
        Ok(count)
    }

    /// The first line of a `for` has been worked out. Give back the text
    /// it built, if it built any.
    fn text_given_back(&mut self, begin: Option<(usize, bool)>) {
        if let Some(mark) = begin.and_then(|begin| self.text_end(begin)) {
            self.text_back(&mark);
        }
    }

    /// The end of a `for` loop. Nothing but the loop ever saw the list
    /// it walked, so that is freed here and now, and not left for the
    /// collector.
    fn close_loop(&mut self, count: &Count) {
        self.scopes.pop();
        self.count_close(count);
        if let Some((items, kind)) = &count.walked {
            self.free(kind, items, Span { line: 0, col: 0 }).expect("a list can be freed");
        }
        if let Some((values, kind)) = &count.values {
            self.free(kind, values, Span { line: 0, col: 0 }).expect("a list can be freed");
        }
        if let Some(held) = &count.held {
            let last = self.load("ptr", held);
            self.free(&Kind::Str, &last, Span { line: 0, col: 0 }).expect("text can be freed");
        }
    }

    // ---------------------------------------------------------------- Match

    /// `match`: the cases are tried in order. Each pattern is a row of
    /// tests, and a test that fails jumps to the next case.
    ///
    /// ```text
    ///   %tag = load i64, ptr %shape          ; case Circle(r)
    ///   %t1 = icmp eq i64 %tag, 0
    ///   br i1 %t1, label %is2, label %case1.1
    /// is2:
    ///   ...read r out of the block, run the body...
    ///   br label %end1
    /// case1.1:                               ; the next case
    /// ```
    fn match_statement(&mut self, subject: &Expr, arms: &[Arm], span: Span) -> Result<(), Error> {
        // With no collector, text built to find the value is needed until
        // a case has been chosen, and is given back as that case begins.
        let begin = self.text_begin();
        let subject = self.expr(subject)?;
        let mark = self.text_end(begin);
        let n = self.number();
        let end = format!("end{n}");
        let mut chosen = false;
        for (index, arm) in arms.iter().enumerate() {
            let next = format!("case{n}.{}", index + 1);
            // The names the pattern gives are variables of this case.
            self.scopes.push(HashMap::new());
            let result = match self.pattern(&arm.pattern, &subject, &next, span) {
                Ok(tested) => self.arm(&arm.body, mark.as_deref()).map(|()| tested),
                Err(error) => Err(error),
            };
            self.scopes.pop();
            let tested = result?;
            self.jump(&end);
            if !tested {
                chosen = true;
                break; // nothing after a pattern that takes everything can run
            }
            self.place(&next);
        }
        // No case was chosen: the text is given back all the same.
        if let (Some(mark), false) = (&mark, chosen) {
            self.text_back(mark);
        }
        self.place(&end);
        Ok(())
    }

    /// The body of one `case`, once its pattern has matched. The names
    /// the pattern gave are in the innermost scope.
    fn arm(&mut self, body: &[Stmt], mark: Option<&str>) -> Result<(), Error> {
        if !self.collector {
            // A name that was given text outlives the text room's copy,
            // and whatever the text was read from: it gets its own.
            let scope = self.scopes.last().expect("a scope");
            let mut names: Vec<String> =
                scope.iter().filter(|(_, variable)| is_text(&variable.kind)).map(|(name, _)| name.clone()).collect();
            names.sort();
            for name in names {
                let variable = self.scopes.last().expect("a scope")[&name].clone();
                let Place::Value(code) = &variable.place else { continue };
                let kept = self.call("ptr", "zeno_text_keep", format!("ptr {code}"));
                let variable = Variable { place: Place::Value(kept), owner: true, ..variable };
                self.scopes.last_mut().expect("a scope").insert(name, variable);
            }
        }
        if let Some(mark) = mark {
            self.text_back(mark);
        }
        self.block(body)?;
        // Unless the body left by itself, free what the names hold.
        if !self.code.ended {
            self.free_owned(self.scopes.len() - 1)?;
        }
        Ok(())
    }

    /// Write the tests of one pattern against `val`, each jumping to
    /// `fail` if it does not hold, and declare the names the pattern
    /// gives. Returns whether there was anything to test.
    fn pattern(&mut self, pattern: &Pattern, val: &Val, fail: &str, span: Span) -> Result<bool, Error> {
        // A value that may be `none` is a pointer that may be null, and a
        // case of an enum matches only a value that is there.
        let there = match &val.kind {
            Kind::Optional(inner) => (**inner).clone(),
            kind => kind.clone(),
        };
        let may_be_none = there != val.kind;
        match pattern {
            Pattern::Wildcard => Ok(false),
            Pattern::Name(name) => match (self.case_in(name, &there), &there) {
                // A bare enum case, such as `Idle`, is compared.
                (Some((owner, index)), Kind::Enum(subject)) if owner == *subject && self.lookup(name).is_none() => {
                    if may_be_none {
                        self.must_be_there(&val.code, fail);
                    }
                    self.tag_is(&val.code, index, fail);
                    Ok(true)
                }
                // Any other name is a new variable holding the value.
                _ => {
                    let variable = Variable { place: Place::Value(val.code.clone()), kind: val.kind.clone(), open: false, owner: false, inner: None };
                    self.scopes.last_mut().expect("a scope").insert(name.clone(), variable);
                    Ok(false)
                }
            },
            Pattern::Literal(literal) => {
                let literal = self.expr(literal)?;
                let Some(common) = joined(&val.kind, &literal.kind) else {
                    return Err(cannot("a pattern of a different type from the value", span));
                };
                let subject = self.coerce(val.clone(), &common, span)?;
                let literal = self.coerce(literal, &common, span)?;
                let same = self.equal(&common, &subject.code, &literal.code, span)?;
                self.must(&same, fail);
                Ok(true)
            }
            Pattern::Variant { name, fields } => {
                let (owner, index) = match (self.case_in(name, &there), &there) {
                    (Some((owner, index)), Kind::Enum(subject)) if owner == *subject => (owner, index),
                    _ => return Err(cannot(format!("the pattern `{name}(...)` here"), span)),
                };
                let carried = self.enums[&owner].cases[index].1.clone();
                if carried.len() != fields.len() {
                    return Err(cannot(format!("the pattern `{name}(...)` with this many parts"), span));
                }
                if may_be_none {
                    self.must_be_there(&val.code, fail);
                }
                self.tag_is(&val.code, index, fail);
                // Now it is known which case this is, and so which struct
                // type describes the block. What it carries comes after
                // the tag.
                let ty = case_type(&owner, name);
                for (position, (field, (_, kind))) in fields.iter().zip(&carried).enumerate() {
                    if *field == Pattern::Wildcard {
                        continue;
                    }
                    let at = self.field_at(&ty, &val.code, position + 1);
                    let inside = self.set(format!("load {}, ptr {at}", ir_type(kind, span)?));
                    self.pattern(field, &Val::new(inside, kind.clone()), fail, span)?;
                }
                Ok(true)
            }
        }
    }

    /// Go on only if `test` holds. Otherwise jump to `fail`.
    fn must(&mut self, test: &str, fail: &str) {
        let held = format!("is{}", self.number());
        self.jump_if(test, &held, fail);
        self.place(&held);
    }

    /// Go on only if `value`, which may be `none`, is not.
    fn must_be_there(&mut self, value: &str, fail: &str) {
        let there = self.set(format!("icmp ne ptr {value}, null"));
        self.must(&there, fail);
    }

    /// Go on only if the enum value at `value` is case number `index`.
    fn tag_is(&mut self, value: &str, index: usize, fail: &str) {
        let tag = self.set(format!("load i64, ptr {value}"));
        let same = self.set(format!("icmp eq i64 {tag}, {index}"));
        self.must(&same, fail);
    }

    // --------------------------------------------------------------- Errors

    /// `try { ... } except Kind as name { ... }`
    ///
    /// This is built on C's `setjmp` and `longjmp`, as in the C backend.
    /// `setjmp` remembers a place in the program and gives 0. If anything
    /// inside the body raises, however deep in function calls, the
    /// runtime's `longjmp` comes back to that same `setjmp`, which this
    /// time gives 1.
    ///
    /// ```text
    ///   call void @zeno_try_open(ptr %try.1)     ; errors come here now
    ///   %t1 = call i32 @_setjmp(ptr %try.1) returns_twice    ; see `SETJMP_TAKES`
    ///   %t2 = icmp eq i32 %t1, 0
    ///   br i1 %t2, label %try1, label %caught1
    /// try1:
    ///   ...the body...
    ///   call void @zeno_try_close(ptr %try.1)    ; finished with no error
    ///   br label %end1
    /// caught1:                                   ; an error jumped back
    ///   %t3 = call zeroext i1 @zeno_caught(ptr @.str.4)
    ///   br i1 %t3, label %except1.1, label %caught1.1
    /// except1.1:
    ///   ...the except block...
    ///   br label %end1
    /// caught1.1:
    ///   call void @zeno_raise_again()            ; not ours: pass it on
    ///   unreachable
    /// ```
    fn try_statement(&mut self, body: &[Stmt], handlers: &[Handler], span: Span) -> Result<(), Error> {
        if self.firmware {
            return self.try_with_values(body, handlers, span);
        }
        let n = self.number();
        // The place to come back to lives on this function's stack. Its
        // size is asked of the runtime (see `FOR_TRY`).
        let place = format!("%try.{n}");
        self.uses("zeno_try_size");
        self.code.slots.push(format!("  %try.size.{n} = call i64 @zeno_try_size()"));
        self.code.slots.push(format!("  {place} = alloca i8, i64 %try.size.{n}, align 16"));

        self.run("zeno_try_open", format!("ptr {place}"));
        self.uses("_setjmp");
        let came = self.set(format!("call i32 @_setjmp(ptr {place}{SETJMP_TAKES}) returns_twice"));
        let first = self.set(format!("icmp eq i32 {came}, 0"));
        let (inside, caught, end) = (format!("try{n}"), format!("caught{n}"), format!("end{n}"));
        self.jump_if(&first, &inside, &caught);

        self.place(&inside);
        self.code.tries.push(place.clone());
        self.code.try_scopes.push(self.scopes.len());
        let result = self.block(body);
        self.code.try_scopes.pop();
        self.code.tries.pop();
        result?;
        // Reaching the end of the body means nothing was raised.
        if !self.code.ended {
            self.run("zeno_try_close", format!("ptr {place}"));
            self.jump(&end);
        }

        // The runtime has already taken this `try` off its list by the
        // time an error arrives here.
        self.place(&caught);
        let mut catches_everything = false;
        for (index, handler) in handlers.iter().enumerate() {
            // What the `except` asks for, and what `as name` then holds.
            let (wanted, holds) = match handler.error.as_deref() {
                None | Some("Error") => (None, Kind::Error),
                Some(class) if self.classes.contains_key(class) => (Some(class), Kind::Class(class.to_string())),
                Some(kind) if emit_c::ERROR_KINDS.contains(&kind) => (Some(kind), Kind::Error),
                Some(other) => return Err(cannot(format!("`except {other}`"), span)),
            };
            let next = format!("caught{n}.{}", index + 1);
            if let Some(kind) = wanted {
                let kind = self.string(kind);
                let ours = self.call("i1", "zeno_caught", format!("ptr {kind}"));
                let handle = format!("except{n}.{}", index + 1);
                self.jump_if(&ours, &handle, &next);
                self.place(&handle);
            }
            self.scopes.push(HashMap::new());
            if let Some(name) = &handler.binding {
                let error = self.set("load ptr, ptr @zeno_raised");
                let value = match holds {
                    // An object that was raised travels inside the error.
                    Kind::Class(_) => {
                        let at = self.field_at(ERROR, &error, 2);
                        self.set(format!("load ptr, ptr {at}"))
                    }
                    _ => error,
                };
                let variable = Variable { place: Place::Value(value), kind: holds, open: false, owner: false, inner: None };
                self.scopes.last_mut().expect("a scope").insert(name.clone(), variable);
            }
            let result = self.block(&handler.body);
            self.scopes.pop();
            result?;
            self.jump(&end);
            if wanted.is_none() {
                catches_everything = true;
                break; // nothing gets past an `except` that takes every error
            }
            self.place(&next);
        }
        if !catches_everything {
            self.run("zeno_raise_again", "");
            self.end("unreachable");
        }
        self.place(&end);
        Ok(())
    }

    // ------------------------------------------------- Tests (decision 60.2)
    //
    // Only in a program built by `zeno test --build`, as in `emit_c`. Each
    // test is a function by now, that may raise any error.

    /// After the program's own statements: each test in turn, with what
    /// it raises caught and reported, and a line for each as it ends.
    fn run_tests(&mut self, tests: &TestBuild) -> Result<(), Error> {
        self.run("zeno_tests_begin", "");
        for (function, name) in &tests.tests {
            let ir_name = self.functions[function].name.clone();
            let shown = self.string(name);
            self.run("zeno_test_begin", format!("ptr {shown}"));
            let n = self.number();
            let (inside, caught, end) = (format!("test{n}"), format!("test.caught{n}"), format!("test.end{n}"));
            if self.firmware {
                let worked = self.set(format!("call i1 {ir_name}()"));
                self.jump_if(&worked, &end, &caught);
            } else {
                let place = format!("%try.{n}");
                self.uses("zeno_try_size");
                self.code.slots.push(format!("  %try.size.{n} = call i64 @zeno_try_size()"));
                self.code.slots.push(format!("  {place} = alloca i8, i64 %try.size.{n}, align 16"));
                self.run("zeno_try_open", format!("ptr {place}"));
                self.uses("_setjmp");
                let came = self.set(format!("call i32 @_setjmp(ptr {place}{SETJMP_TAKES}) returns_twice"));
                let first = self.set(format!("icmp eq i32 {came}, 0"));
                self.jump_if(&first, &inside, &caught);
                self.place(&inside);
                self.ins(format!("call void {ir_name}()"));
                self.run("zeno_try_close", format!("ptr {place}"));
                self.jump(&end);
            }
            self.place(&caught);
            self.run("zeno_test_uncaught", "");
            self.jump(&end);
            self.place(&end);
            self.run("zeno_test_end", "");
        }
        Ok(())
    }

    /// `assert`, in a test. The condition is worked out once. If it is
    /// false, the runtime is given the report: the text after the comma,
    /// worked out only now, and for a comparison what was on each side.
    /// Then the test is left as `return` leaves it: through every `try`,
    /// which does not catch it, and freeing what the test owns.
    fn test_assert(&mut self, condition: &Expr, message: Option<&Expr>, span: Span) -> Result<(), Error> {
        let n = self.number();
        let failed = self.slot(format!("%assert.{n}"), "i1");
        self.store("i1", "false", &failed);
        let begin = self.text_begin();
        let own = self.own_begin();
        let (fails, holds_label) = (format!("assert.false{n}"), format!("assert.true{n}"));
        let place = at(condition.span);
        match &condition.kind {
            ExprKind::Binary { op, left, right }
                if matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Lt
                        | BinaryOp::LtEq
                        | BinaryOp::Gt
                        | BinaryOp::GtEq
                        | BinaryOp::Is
                        | BinaryOp::IsNot
                        | BinaryOp::In
                        | BinaryOp::NotIn
                ) =>
            {
                let (left, right, holds) = if matches!(op, BinaryOp::Is | BinaryOp::IsNot) {
                    // `x is none`: the right is `none`, as written.
                    let left = self.expr(left)?;
                    let right = self.expr(right)?;
                    let is = *op == BinaryOp::Is;
                    let holds = match &left.kind {
                        Kind::Optional(_) => {
                            let there = self.is_there(&left, span)?;
                            if is {
                                self.set(format!("xor i1 {there}, true"))
                            } else {
                                there
                            }
                        }
                        other => ((*other == Kind::None) == is).to_string(),
                    };
                    (left, right, holds)
                } else {
                    let left = self.expr(left)?;
                    let right = self.expr(right)?;
                    let holds = match op {
                        BinaryOp::In | BinaryOp::NotIn => self.contains(*op, left.clone(), right.clone(), span)?.code,
                        _ => self.compare(*op, &left, &right, span)?,
                    };
                    (left, right, holds)
                };
                self.jump_if(&holds, &holds_label, &fails);
                self.place(&fails);
                let text = self.assert_text(message)?;
                let (left, right) = (self.assert_shown(&left, span)?, self.assert_shown(&right, span)?);
                let op = self.string(op.text());
                self.run("zeno_test_false", format!("ptr {text}, ptr {op}, ptr {left}, ptr {right}, {place}"));
            }
            _ => {
                let holds = self.expr(condition)?;
                self.jump_if(&holds.code, &holds_label, &fails);
                self.place(&fails);
                let text = self.assert_text(message)?;
                self.run("zeno_test_false", format!("ptr {text}, ptr null, ptr null, ptr null, {place}"));
            }
        }
        self.store("i1", "true", &failed);
        self.jump(&holds_label);
        self.place(&holds_label);
        self.own_end(own, true)?;
        if let Some(mark) = self.text_end(begin) {
            self.text_back(&mark);
        }
        self.leave_test(&failed, span)
    }

    /// One side of a comparison, as a failed `assert` shows it: as `print`
    /// shows it inside a list, so that text has its quotes.
    fn assert_shown(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        match val.kind {
            Kind::None => Ok(self.string("none")),
            _ => self.repr(val, span),
        }
    }

    /// The text after the comma of an `assert`, or `null`.
    fn assert_text(&mut self, message: Option<&Expr>) -> Result<String, Error> {
        match message {
            Some(message) => Ok(self.expr(message)?.code),
            None => Ok("null".to_string()),
        }
    }

    /// After a failed `assert`: leave the test as `return` would.
    fn leave_test(&mut self, failed: &str, span: Span) -> Result<(), Error> {
        let n = self.number();
        let (leave, go_on) = (format!("assert.leave{n}"), format!("assert.on{n}"));
        let failed = self.set(format!("load i1, ptr {failed}"));
        self.jump_if(&failed, &leave, &go_on);
        self.place(&leave);
        self.statement(&Stmt { kind: StmtKind::Return(None), span })?;
        self.place(&go_on);
        Ok(())
    }

    /// `assert raises Kind { ... }`, in a test: a `try` whose body must
    /// raise that kind. Raising nothing, or another kind, is reported as
    /// `zeno test` reports it, and leaves the test.
    fn test_assert_raises(&mut self, kind: &str, body: &[Stmt], span: Span) -> Result<(), Error> {
        let wanted = match kind {
            "Error" => None,
            class if self.classes.contains_key(class) => Some(class),
            kind if emit_c::ERROR_KINDS.contains(&kind) => Some(kind),
            other => return Err(cannot(format!("`assert raises {other}`"), span)),
        };
        let n = self.number();
        let failed = self.slot(format!("%raises.{n}"), "i1");
        self.store("i1", "false", &failed);
        let shown = self.string(kind);
        let place_of = at(span);
        let (inside, caught, end) = (format!("raises.try{n}"), format!("raises.caught{n}"), format!("raises.end{n}"));
        let other = format!("raises.other{n}");
        if !self.firmware {
            let place = format!("%try.{n}");
            self.uses("zeno_try_size");
            self.code.slots.push(format!("  %try.size.{n} = call i64 @zeno_try_size()"));
            self.code.slots.push(format!("  {place} = alloca i8, i64 %try.size.{n}, align 16"));
            self.run("zeno_try_open", format!("ptr {place}"));
            self.uses("_setjmp");
            let came = self.set(format!("call i32 @_setjmp(ptr {place}{SETJMP_TAKES}) returns_twice"));
            let first = self.set(format!("icmp eq i32 {came}, 0"));
            self.jump_if(&first, &inside, &caught);
            self.place(&inside);
            self.code.tries.push(place.clone());
            self.code.try_scopes.push(self.scopes.len());
            let result = self.block(body);
            self.code.try_scopes.pop();
            self.code.tries.pop();
            result?;
            if !self.code.ended {
                self.run("zeno_try_close", format!("ptr {place}"));
                self.run("zeno_test_raised_nothing", format!("ptr {shown}, {place_of}"));
                self.store("i1", "true", &failed);
                self.jump(&end);
            }
            self.place(&caught);
            if let Some(wanted) = wanted {
                let wanted = self.string(wanted);
                let ours = self.call("i1", "zeno_caught", format!("ptr {wanted}"));
                self.jump_if(&ours, &end, &other);
                self.place(&other);
                let raised = self.set("load ptr, ptr @zeno_raised");
                self.run("zeno_test_raised_other", format!("ptr {shown}, ptr {raised}, {place_of}"));
                self.store("i1", "true", &failed);
            }
            self.jump(&end);
            self.place(&end);
            return self.leave_test(&failed, span);
        }

        // A firmware build: the error travels as a value, and a failure in
        // the body comes forward to the label (see `try_with_values`).
        let except = format!("raises.except{n}");
        self.code.tries.push(except.clone());
        self.code.arriving.push(BTreeSet::new());
        self.code.try_scopes.push(self.scopes.len());
        let text_top = (!self.collector).then(|| self.set("load i64, ptr @zeno_text_top"));
        let result = self.block(body);
        self.code.try_scopes.pop();
        self.code.tries.pop();
        self.code.arriving.pop();
        result?;
        if !self.code.ended {
            self.run("zeno_test_raised_nothing", format!("ptr {shown}, {place_of}"));
            self.store("i1", "true", &failed);
        }
        self.place(&except);
        if let Some(mark) = &text_top {
            self.text_back(mark);
        }
        let raised = self.set("load ptr, ptr @zeno_raised");
        let came = self.set(format!("icmp ne ptr {raised}, null"));
        self.jump_if(&came, &caught, &end);
        self.place(&caught);
        self.ins("store ptr null, ptr @zeno_raised");
        if let Some(wanted) = wanted {
            let wanted = self.string(wanted);
            let ours = self.call("i1", "zeno_is", format!("ptr {raised}, ptr {wanted}"));
            let after = format!("raises.after{n}");
            self.jump_if(&ours, &after, &other);
            self.place(&other);
            self.run("zeno_test_raised_other", format!("ptr {shown}, ptr {raised}, {place_of}"));
            self.store("i1", "true", &failed);
            self.jump(&after);
            self.place(&after);
        }
        if !self.collector {
            self.run("zeno_error_drop", format!("ptr {raised}"));
        }
        self.jump(&end);
        self.place(&end);
        self.leave_test(&failed, span)
    }

    /// Before a `return`, `break` or `continue` jumps out of `try`
    /// blocks, tell the runtime that those blocks are over. `keep` is how
    /// many of the open ones stay open.
    fn leave_tries(&mut self, keep: usize) {
        // In a firmware build a `try` leaves nothing behind to undo.
        if self.firmware {
            return;
        }
        if let Some(outermost) = self.code.tries.get(keep).cloned() {
            self.run("zeno_try_close", format!("ptr {outermost}"));
        }
    }

    /// `raise`. The runtime jumps to the nearest `try` that is running,
    /// or stops the program if there is none. Either way it does not
    /// come back here, which is what `unreachable` says.
    fn raise(&mut self, value: &Expr, span: Span) -> Result<(), Error> {
        let begin = self.text_begin();
        // With no collector `raise` takes the error: whoever held it
        // holds nothing now, and the `except` that catches it is its
        // next owner.
        let val = self.expr(value)?;
        self.stays(&val, value.span)?;
        let error = match &val.kind {
            Kind::Error => val.code,
            // An object of a class can be raised too. It travels inside
            // an error value whose kind is the class's name.
            Kind::Class(name) => {
                let kind = self.string(name);
                let shown = self.text(&val, span)?;
                if self.collector {
                    self.call("ptr", "zeno_error_new", format!("ptr {kind}, ptr {shown}, ptr {}", val.code))
                } else {
                    // The `except` that catches it frees the object, and
                    // the text the object keeps, through this function.
                    let drop = self.drop_class(name, span)?;
                    self.call("ptr", "zeno_error_of", format!("ptr {kind}, ptr {shown}, ptr {}, ptr {drop}", val.code))
                }
            }
            _ => return Err(cannot("`raise` with anything but an error or an object", span)),
        };
        if self.firmware {
            // Note the error, then go where a failed call would go.
            let kind = match (&val.kind, &value.kind) {
                (Kind::Class(name), _) => name.clone(),
                // `raise ValueError("...")` says its kind. An error held
                // in a variable could be any kind.
                (_, ExprKind::Call { callee, .. }) => match &callee.kind {
                    ExprKind::Name(name) if emit_c::ERROR_KINDS.contains(&name.as_str()) => name.clone(),
                    _ => "Error".to_string(),
                },
                _ => "Error".to_string(),
            };
            self.run("zeno_note", format!("ptr {error}, {}", at(span)));
            // The error has its own copy of its message by now.
            if let Some(mark) = self.text_end(begin) {
                self.text_back(&mark);
            }
            return self.failed(&[kind], span);
        }
        // The jump leaves at least the blocks opened since the nearest
        // `try` in this function, or all of the function's if there is
        // none. Free what their owners hold first. (The functions the
        // jump passes through on its way are not asked: what they own is
        // left to the collector.)
        let inside = self.code.try_scopes.last().copied().unwrap_or(self.base());
        self.free_owned(inside)?;
        self.run("zeno_raise", format!("ptr {error}, {}", at(span)));
        self.end("unreachable");
        Ok(())
    }

    // ------------------------------------------- Errors as values (firmware)
    //
    // `zeno build --llvm --firmware`. Nothing jumps out of a function. A
    // function that can raise gives back an `i1`: 1 when it worked, 0
    // when it raised. Its answer goes out through one more parameter, a
    // pointer. `raise` notes the error in the runtime's `zeno_raised` and
    // gives back 0, and every call to such a function is followed by a
    // test. This is `docs/firmware.md`, written in IR in place of C.

    /// Something here has failed: a `raise`, or a call that has just
    /// given back 0. The error is already noted in `zeno_raised`. Leave
    /// the block for where the error goes next:
    ///
    /// - inside a `try`, its `except` blocks, further down this function;
    /// - in a function that can raise, back to the caller, with 0;
    /// - at the top of the file, nobody is left to catch it.
    ///
    /// `kinds` is what may have been raised, so that the `try` around
    /// this spot knows what can reach it.
    fn failed(&mut self, kinds: &[String], span: Span) -> Result<(), Error> {
        if let Some(arriving) = self.code.arriving.last_mut() {
            arriving.extend(kinds.iter().cloned());
        }
        // Leaving this way ends blocks early, as `break` and `return` do:
        // what their owners hold is freed on the way out.
        if let Some(label) = self.code.tries.last().cloned() {
            let inside = self.code.try_scopes.last().copied().unwrap_or(self.scopes.len());
            self.free_owned_raised(inside)?;
            self.jump(&label);
        } else if self.code.fallible {
            self.free_owned_raised(self.base())?;
            self.end("ret i1 false");
        } else if !self.code.in_function {
            // Nobody catches it. The resources of the top of the file are
            // released on its way out, as `zeno run` releases them; the
            // rest is left as it is (decision 59.1).
            if !self.code.resources.is_empty() {
                let kept = self.keep_raised();
                let held: Vec<(usize, String)> = self.code.resources.iter().rev().cloned().collect();
                for (depth, name) in held {
                    self.release(depth, &name, span)?;
                }
                self.put_back_raised(kept);
            }
            self.run("zeno_uncaught", "");
            self.end("unreachable");
        } else {
            // The checker makes a function say what it raises, and a
            // function written where a value goes that raises is made as
            // one that can.
            return Err(cannot("an error raised in a function that does not say it raises", span));
        }
        Ok(())
    }

    /// Is this the type of a function that is called as one that can
    /// raise? Only in a firmware build, where it gives back an `i1`
    /// (5.5l). Every other build calls both kinds alike (56.5).
    fn raising(&self, kind: &FunctionKind) -> bool {
        self.firmware && !kind.raises.is_empty()
    }

    /// `worked` is the `i1` that a call which can fail gave back. If it
    /// is 0 the error goes on its way; if 1, carry on.
    ///
    /// ```text
    ///   %t4 = call zeroext i1 @z_parse(ptr %t3, ptr %answer.2)
    ///   br i1 %t4, label %worked2, label %failed2
    /// failed2:
    ///   ret i1 false                    ; or: br label %except1
    /// worked2:
    ///   %t5 = load double, ptr %answer.2
    /// ```
    fn tested(&mut self, worked: &str, kinds: &[String], span: Span) -> Result<(), Error> {
        let n = self.number();
        let (fine, bad) = (format!("worked{n}"), format!("failed{n}"));
        self.jump_if(worked, &fine, &bad);
        self.place(&bad);
        self.failed(kinds, span)?;
        self.place(&fine);
        Ok(())
    }

    /// A call, in a firmware build, to something that gives back an `i1`
    /// and puts its answer where its last argument points. `call` is the
    /// call up to that last argument, with no closing bracket.
    fn checked_call(&mut self, call: String, returns: &Kind, kinds: &[String], span: Span) -> Result<Val, Error> {
        if *returns == Kind::None {
            let worked = self.set(format!("{call})"));
            self.tested(&worked, kinds, span)?;
            return Ok(Val::nothing());
        }
        // A place on this function's stack for the answer.
        let ty = ir_type(returns, span)?;
        let n = self.number();
        let answer = self.slot(format!("%answer.{n}"), ty);
        let comma = if call.ends_with('(') { "" } else { ", " };
        let worked = self.set(format!("{call}{comma}ptr {answer})"));
        self.tested(&worked, kinds, span)?;
        let val = Val::new(self.set(format!("load {ty}, ptr {answer}")), returns.clone());
        Ok(self.came_back(val))
    }

    /// `try` / `except` in a firmware build. Each thing in the body that
    /// can fail is followed by a test, and a failure goes forward to the
    /// `except` blocks. So does the end of the body, and there the first
    /// question is whether anything failed at all:
    ///
    /// ```text
    ///   ...the body, with `br label %except1` after each failure...
    ///   br label %except1
    /// except1:
    ///   %t6 = load ptr, ptr @zeno_raised
    ///   %t7 = icmp ne ptr %t6, null             ; did the body fail?
    ///   br i1 %t7, label %caught1, label %end1
    /// caught1:
    ///   store ptr null, ptr @zeno_raised        ; it is being dealt with
    ///   %t8 = call zeroext i1 @zeno_is(ptr %t6, ptr @.str.4)
    ///   br i1 %t8, label %except1.1, label %caught1.1
    /// except1.1:
    ///   ...the except block...
    ///   br label %end1
    /// caught1.1:
    ///   store ptr %t6, ptr @zeno_raised         ; not ours: pass it on
    ///   ret i1 false
    /// end1:
    /// ```
    fn try_with_values(&mut self, body: &[Stmt], handlers: &[Handler], span: Span) -> Result<(), Error> {
        let n = self.number();
        let (except, caught, end) = (format!("except{n}"), format!("caught{n}"), format!("end{n}"));
        self.code.tries.push(except.clone());
        self.code.arriving.push(BTreeSet::new());
        self.code.try_scopes.push(self.scopes.len());
        // With no collector: an error can leave a statement half way
        // through, with its text still in the text room. Whatever the
        // body leaves there is given back where the error is caught.
        let text_top = (!self.collector).then(|| self.set("load i64, ptr @zeno_text_top"));
        let result = self.block(body);
        self.code.try_scopes.pop();
        self.code.tries.pop();
        let arrived = self.code.arriving.pop().unwrap_or_default();
        result?;

        // Only an error that travels as a value can reach an `except`. A
        // division by zero or a bad index stops a firmware build instead,
        // and an `except` waiting for one would wait for ever.
        const WHY: &str = "In a firmware build, an error the program raises itself (and `int` or `float` of text) travels as a value and can be caught. A division by zero, a bad index, a key that is not in a dict or an overflow stops the program. Test for it before it happens";
        if arrived.is_empty() {
            return Err(firmware_cannot(format!("nothing in this `try` can be caught. {WHY}"), span));
        }
        let anything = arrived.contains("Error");

        self.place(&except);
        if let Some(mark) = &text_top {
            self.text_back(mark);
        }
        let raised = self.set("load ptr, ptr @zeno_raised");
        let failed = self.set(format!("icmp ne ptr {raised}, null"));
        self.jump_if(&failed, &caught, &end);
        self.place(&caught);
        self.ins("store ptr null, ptr @zeno_raised");

        let mut handled: Vec<&str> = Vec::new();
        let mut catches_everything = false;
        for (index, handler) in handlers.iter().enumerate() {
            let (wanted, holds) = match handler.error.as_deref() {
                None | Some("Error") => (None, Kind::Error),
                Some(class) if self.classes.contains_key(class) => (Some(class), Kind::Class(class.to_string())),
                Some(kind) if emit_c::ERROR_KINDS.contains(&kind) => (Some(kind), Kind::Error),
                Some(other) => return Err(cannot(format!("`except {other}`"), span)),
            };
            let next = format!("caught{n}.{}", index + 1);
            if let Some(kind) = wanted {
                // An `except` for a class that nothing here raises is idle
                // in every kind of build. One for a built-in kind is most
                // likely waiting for the runtime to raise it.
                if !anything && !arrived.contains(kind) && !self.classes.contains_key(kind) {
                    return Err(firmware_cannot(format!("nothing in this `try` can reach `except {kind}`. {WHY}"), span));
                }
                handled.push(kind);
                let kind = self.string(kind);
                let ours = self.call("i1", "zeno_is", format!("ptr {raised}, ptr {kind}"));
                let handle = format!("except{n}.{}", index + 1);
                self.jump_if(&ours, &handle, &next);
                self.place(&handle);
            }
            self.scopes.push(HashMap::new());
            // With no collector this block has received the error and
            // owns it. However the block is left, the error is freed on
            // the way out, unless the block gave it on: kept it,
            // returned it or raised it again.
            let named = handler.binding.is_some();
            if !self.collector && !(named && holds == Kind::Error) {
                self.hold(Kind::Error, &raised);
            }
            if let Some(name) = &handler.binding {
                let value = match holds {
                    // An object that was raised travels inside the error.
                    Kind::Class(_) => {
                        let at = self.field_at(ERROR, &raised, 2);
                        let object = self.set(format!("load ptr, ptr {at}"));
                        if !self.collector {
                            // The name owns the object from here, so it
                            // is taken out of the error.
                            self.store("ptr", "null", &at);
                        }
                        object
                    }
                    _ => raised.clone(),
                };
                let variable = match self.collector {
                    true => Variable { place: Place::Value(value), kind: holds, open: false, owner: false, inner: None },
                    // The name is an owner, so it is a slot: giving the
                    // error on leaves null behind.
                    false => {
                        let slot = self.slot(format!("%caught.{n}.{}", index + 1), "ptr");
                        self.store("ptr", &value, &slot);
                        Variable { place: Place::Slot(slot), kind: holds, open: false, owner: true, inner: None }
                    }
                };
                self.scopes.last_mut().expect("a scope").insert(name.clone(), variable);
            }
            let mut result = self.block(&handler.body);
            if result.is_ok() && !self.code.ended {
                result = self.free_owned(self.scopes.len() - 1);
            }
            self.scopes.pop();
            result?;
            self.jump(&end);
            if wanted.is_none() {
                catches_everything = true;
                break; // nothing gets past an `except` that takes every error
            }
            self.place(&next);
        }
        if !catches_everything {
            // What no `except` here takes carries on outward.
            self.ins(format!("store ptr {raised}, ptr @zeno_raised"));
            let passed_on: Vec<String> =
                arrived.iter().filter(|kind| !handled.contains(&kind.as_str())).cloned().collect();
            if passed_on.is_empty() {
                // Every kind the body can raise has an `except` above, so
                // this cannot happen. LLVM cannot see that, and a block
                // has to end somehow.
                self.run("zeno_uncaught", "");
                self.end("unreachable");
            } else {
                self.failed(&passed_on, span)?;
            }
        }
        self.place(&end);
        Ok(())
    }

    // ---------------------------------------------------------- Expressions

    /// An expression that must be a bool, for `if` and `while`.
    fn condition(&mut self, expr: &Expr) -> Result<String, Error> {
        // With no collector: if working it out builds text, the answer
        // is one bit by the time it is known, and the text is given back
        // before anything else runs.
        let begin = self.text_begin();
        // The same goes for a value that the condition alone owns.
        let own = self.own_begin();
        let val = self.expr(expr)?;
        self.own_end(own, true)?;
        if let Some(mark) = self.text_end(begin) {
            self.text_back(&mark);
        }
        match val.kind {
            Kind::Bool => Ok(val.code),
            _ => Err(cannot(format!("{} used as a condition", described(&val.kind)), expr.span)),
        }
    }

    /// `expr`, for a place that knows what kind it wants. Only an empty
    /// list needs the help: `[]` alone does not say what it will hold.
    fn expr_as(&mut self, expr: &Expr, expected: Option<&Kind>) -> Result<Val, Error> {
        match (&expr.kind, expected) {
            (ExprKind::List(items), Some(kind @ Kind::List(_))) if items.is_empty() => {
                let val = Val::new(self.call("ptr", "zeno_list_new", "i64 0"), kind.clone()).made();
                Ok(self.belongs(val, expr))
            }
            (ExprKind::Dict(entries), Some(kind @ Kind::Dict(key, value))) if entries.is_empty() => {
                let kinds = self.dict_kinds(key, value, expr.span)?;
                let val = Val::new(self.call("ptr", "zeno_dict_new", kinds), kind.clone()).made();
                Ok(self.belongs(val, expr))
            }
            _ => self.expr(expr),
        }
    }

    /// Write the instructions that work an expression out, and return
    /// the value they leave.
    fn expr(&mut self, expr: &Expr) -> Result<Val, Error> {
        let val = self.value(expr)?;
        let val = self.belongs(val, expr);
        // For a build with no collector: text that was not already there
        // has just been built, and the statement will have to give it
        // back. Of several values, the first comes back as one value
        // does, and the others by copy (decision 57).
        let text = match &val.kind {
            Kind::Several(kinds) => emit_c::is_text(&kinds[0]),
            Kind::Optional(inner) => **inner == Kind::Str,
            kind => *kind == Kind::Str,
        };
        if text && !val.fixed && !self.collector {
            self.code.built = true;
        }
        Ok(val)
    }

    fn value(&mut self, expr: &Expr) -> Result<Val, Error> {
        let span = expr.span;
        Ok(match &expr.kind {
            // A resource given away (decision 59.1): as an owner gives its
            // value away (see `name`).
            ExprKind::Give(name) => match self.lookup(name).cloned() {
                Some(Variable { place: Place::Slot(slot), kind, .. }) => {
                    let ty = ir_type(&kind, span)?;
                    let taken = self.load(ty, &slot);
                    self.store(ty, "null", &slot);
                    Val::new(taken, kind)
                }
                _ => return Err(cannot(format!("`{name}` given away here"), span)),
            },
            // Read by `return` and `receive` themselves (decision 57).
            ExprKind::Several(_) => return Err(cannot("several values written anywhere but after `return` or `=`", span)),
            // The lexer lets one number through that is one too big for an
            // int, and `zeno run` reports it when it gets there.
            ExprKind::Int(n) if *n > i64::MAX as u64 => return Err(cannot("a whole number this big", span)),
            ExprKind::Int(n) => Val::new(n.to_string(), Kind::Int),
            ExprKind::Float(x) => Val::new(float_code(*x), Kind::Float),
            ExprKind::Bool(b) => Val::new(b.to_string(), Kind::Bool),
            ExprKind::Str(text) => Val::new(self.string(text), Kind::Str).fixed(),
            // A block of C is C, and this build writes none (61.7).
            ExprKind::Inline { language, .. } => {
                return Err(Error::new(
                    format!(
                        "`zeno build --llvm` cannot take an `inline {language}` block: it writes LLVM, and the block is not that. \
                         {}",
                        if language == "c" { "`zeno build` without `--llvm` writes C, and takes it" } else { "`zeno run` takes it" }
                    ),
                    span,
                ));
            }
            ExprKind::FormatStr(parts) => self.format_string(parts, span)?,
            // What `none` is in IR depends on where it is going, so it is
            // settled there. See `coerce`.
            ExprKind::None => Val::new("null", Kind::None),
            ExprKind::Name(name) => self.name(name, expr)?,

            // A quantity is a float. Its unit has done its work in the
            // checker and costs nothing in the compiled program.
            ExprKind::Unit { value, unit } => {
                let val = self.expr(value)?;
                if !matches!(val.kind, Kind::Int | Kind::Float) {
                    return Err(cannot(format!("{} with a unit", described(&val.kind)), span));
                }
                Val::new(self.double(&val), Kind::Quantity(unit.clone()))
            }

            ExprKind::Unary { op: UnaryOp::Neg, operand } => match &operand.kind {
                // `-5` is a number, not a sum to be worked out.
                ExprKind::Int(n) if *n <= i64::MAX as u64 => Val::new(format!("-{n}"), Kind::Int),
                _ => {
                    let val = self.expr(operand)?;
                    match val.kind {
                        Kind::Int => Val::new(self.checked("ssub", "0", &val.code, span), Kind::Int),
                        Kind::Float | Kind::F32 | Kind::Quantity(_) => {
                            Val::new(self.set(format!("fneg double {}", val.code)), val.kind)
                        }
                        _ => return Err(cannot(format!("`-` in front of {}", described(&val.kind)), span)),
                    }
                }
            },
            // Every bit turned over: exclusive-or with a number that is
            // all ones, which is -1.
            ExprKind::Unary { op: UnaryOp::BitNot, operand } => {
                let val = self.expr(operand)?;
                if !matches!(val.kind, Kind::Int | Kind::U64) {
                    return Err(cannot(format!("`~` in front of {}", described(&val.kind)), span));
                }
                Val::new(self.set(format!("xor i64 {}, -1", val.code)), val.kind)
            }
            ExprKind::Unary { op: UnaryOp::Not, operand } => {
                let test = self.condition(operand)?;
                // There is no "not" instruction. Exclusive-or with 1 flips the bit.
                Val::new(self.set(format!("xor i1 {test}, true")), Kind::Bool)
            }

            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
                self.logic(*op, left, right)?
            }
            ExprKind::Binary { op: op @ (BinaryOp::Is | BinaryOp::IsNot), left, .. } => self.is_none(*op, left)?,
            ExprKind::Binary { op, left, right } => {
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                match op {
                    BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::FloorDiv
                    | BinaryOp::Mod
                    | BinaryOp::Pow
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr => self.arithmetic(*op, left, right, span)?,
                    BinaryOp::In | BinaryOp::NotIn => self.contains(*op, left, right, span)?,
                    _ => Val::new(self.compare(*op, &left, &right, span)?, Kind::Bool),
                }
            }
            ExprKind::Compare { first, rest } => self.chain(first, rest)?,

            // A call of a generic goes to one of its copies, and the
            // checker noted which: `first([1, 2])` calls `first[int]`.
            ExprKind::Call { callee, args } if self.types.copy_called(expr).is_some() => {
                let copy = self.types.copy_called(expr).unwrap_or_default().to_string();
                if let Some(entry) = self.functions.get(&copy).cloned() {
                    self.call_named(&entry, args, span)?
                } else if self.classes.contains_key(&copy) {
                    self.construct(&copy, args, span)?
                } else if self.copied_enums.contains(&copy) {
                    // `Just(3)`: the case of the copy `Maybe[int]`.
                    let index = self.copied_case(&copy, callee, span)?;
                    self.construct_case(&copy, index, args, span)?
                } else {
                    return Err(cannot(format!("`{copy}`, a generic that is not at the top of the file,"), span));
                }
            }
            ExprKind::Call { callee, args } => {
                self.makes_bytes = self.types.makes_bytes(expr);
                self.call_expr(callee, args, span)?
            }
            // `counter.bump` as a value: the function the checker wrote
            // for it (decision 56.9).
            ExprKind::Field { object, name, .. } if self.types.method_values.contains_key(&(expr as *const Expr as usize)) => {
                let written = self.types.method_values[&(expr as *const Expr as usize)].clone();
                self.method_value(object, name, &written)?
            }
            ExprKind::Field { object, name, .. } => self.field(object, name, span)?,
            ExprKind::Take(place) => self.take_out(place)?,
            ExprKind::Index { object, index } => {
                let list = self.expr(object)?;
                let position = self.expr(index)?;
                // `ages["Ada"]`: the runtime finds the key and gives its
                // value, or fails with a KeyError.
                if let Kind::Dict(_, value) = &list.kind {
                    let key = self.to_slot(&position, span)?;
                    let found = self.call("i64", "zeno_dict_at", format!("ptr {}, i64 {key}, {}", list.code, at(span)));
                    let val = self.from_slot(value, &found, span)?;
                    return Ok(self.lend(val));
                }
                let args = format!("ptr {}, i64 {}, {}", list.code, position.code, at(span));
                match &list.kind {
                    _ if position.kind != Kind::Int => return Err(cannot("`[ ]` with anything but an int inside", span)),
                    Kind::Str => Val::new(self.call("ptr", "zeno_char_at", args), Kind::Str),
                    // The runtime checks the index and gives back the
                    // address of that item's slot.
                    Kind::List(item) => {
                        let place = self.list_place(&list.code, &position.code, item, span);
                        let slot = self.load_item(&place, item);
                        let val = self.from_slot(item, &slot, span)?;
                        self.lend(val)
                    }
                    // Through a pointer: the address of that item, and
                    // a load, with no check. A sized item is made wide
                    // again, like a field.
                    Kind::Pointer(pointee) => {
                        let item = pointee.ir_type();
                        let place = self.set(format!("getelementptr {item}, ptr {}, i64 {}", list.code, position.code));
                        let read = self.set(format!("load {item}, ptr {place}"));
                        let code = match pointee {
                            Pointee::Sized(width) => {
                                let widen = if width.signed { "sext" } else { "zext" };
                                self.set(format!("{widen} {item} {read} to i64"))
                            }
                            _ => read,
                        };
                        Val::new(code, pointee.kind())
                    }
                    _ => return Err(cannot(format!("`[ ]` on {}", described(&list.kind)), span)),
                }
            }
            ExprKind::List(items) => self.list(expr, items, span)?.made(),
            ExprKind::Dict(entries) => self.dict(expr, entries, span)?.made(),
            ExprKind::ListComp { element, variable, iterable, condition } => {
                let bytes = self.types.makes_bytes(expr);
                self.comprehension(element, variable, iterable, condition.as_deref(), bytes, span)?.made()
            }
            ExprKind::Function(decl) => self.closure(decl, span)?,
        })
    }

    /// A name used as a value: a variable, an enum case that carries
    /// nothing, `pi`, or one of the program's functions.
    fn name(&mut self, name: &str, expr: &Expr) -> Result<Val, Error> {
        let span = expr.span;
        let Some(variable) = self.lookup(name).cloned() else {
            // `Nothing` of a generic enum: the case of the copy the
            // checker found for this place (decision 58.1).
            if let Some(owner) = self.types.copy_called(expr).map(str::to_string) {
                let index = self.copied_case(&owner, expr, span)?;
                if !self.enums[&owner].cases[index].1.is_empty() {
                    return Err(cannot("an enum case that carries values, used without them", span));
                }
                return Ok(Val::new(bare_case(&owner, name), Kind::Enum(owner)));
            }
            if let Some((owner, index)) = self.cases.get(name).cloned() {
                if !self.enums[&owner].cases[index].1.is_empty() {
                    return Err(cannot("an enum case that carries values, used without them", span));
                }
                return Ok(Val::new(bare_case(&owner, name), Kind::Enum(owner)));
            }
            if let (Some(number), true) =
                (emit_c::math_constant(name), self.imported.get(name).is_some_and(|module| module == "math"))
            {
                return Ok(Val::new(float_code(number), Kind::Float));
            }
            // `from board import led`: a pin is its number.
            if name == "led" && self.imported.get(name).is_some_and(|module| module == "board") {
                return Ok(Val::new(LED.to_string(), Kind::Pin));
            }
            // `from board import pull_up`: the str of its name (45.3).
            if matches!(name, "pull_up" | "pull_down" | "rising" | "falling" | "either") && self.imported.get(name).is_some_and(|module| module == "board") {
                return Ok(Val::new(self.string(name), Kind::Str).fixed());
            }
            if self.functions.contains_key(name) {
                return self.function_value(name, span);
            }
            return Err(cannot(format!("`{name}` used this way"), span));
        };
        if let (Some((decl, _)), false) = (&variable.inner, self.collector) {
            return Err(emit_c::inner_as_value(&decl.name, span));
        }
        let val = match &variable.place {
            Place::Value(code) => Val::new(code.clone(), variable.kind.clone()),
            // The owner gives its value away here. Take the address out
            // and leave null behind, so that the end of the owner's block
            // finds nothing to free.
            Place::Slot(slot) if self.types.moves(expr) => {
                let ty = ir_type(&variable.kind, span)?;
                let taken = self.load(ty, slot);
                self.store(ty, "null", slot);
                return Ok(Val::new(taken, variable.kind.clone()));
            }
            Place::Slot(slot) => {
                let ty = ir_type(&variable.kind, span)?;
                Val::new(self.load(ty, slot), variable.kind.clone())
            }
        };
        // Whatever a variable holds was there before this statement
        // began. If a function could give the variable new text, what it
        // holds now is lent.
        let val = self.present(val, expr, variable.open)?;
        Ok(if variable.open { self.lend(val) } else { val.fixed() })
    }

    /// An int as a float, for arithmetic that mixes the two.
    fn double(&mut self, val: &Val) -> String {
        match val.kind {
            // "signed int to floating point".
            Kind::Int => self.set(format!("sitofp i64 {} to double", val.code)),
            // "unsigned int to floating point".
            Kind::U64 => self.set(format!("uitofp i64 {} to double", val.code)),
            _ => val.code.clone(),
        }
    }

    /// The value as the double an f32 is, for a sum on an f32: an int
    /// becomes a float and then the nearest f32 (decision 41.2).
    fn single(&mut self, val: &Val) -> String {
        match val.kind {
            Kind::Int => {
                let wide = self.set(format!("sitofp i64 {} to double", val.code));
                self.rounded(&wide)
            }
            _ => val.code.clone(),
        }
    }

    /// A double rounded to the nearest f32, and held as a double again.
    /// The double sum of two f32s, rounded so, is the f32 sum exactly,
    /// and LLVM makes the pair one `float` instruction where it can.
    fn rounded(&mut self, double: &str) -> String {
        let near = self.set(format!("fptrunc double {double} to float"));
        self.set(format!("fpext float {near} to double"))
    }

    /// `+`, `-` or `*` on two ints, with the check Zeno promises.
    ///
    /// A plain `add` would wrap round on overflow. This asks for the
    /// result together with the processor's overflow flag, and leaves for
    /// a block that reports the error if the flag is set. The runtime
    /// function does not come back, which is what `unreachable` says.
    fn checked(&mut self, op: &'static str, left: &str, right: &str, span: Span) -> String {
        // `--release`: the plain instruction, which wraps round. With no
        // `nsw` flag LLVM may not assume it does not.
        // `--fast`: with `nsw` ("no signed wrap"), which is what a C
        // compiler says of a C program's `+`: it does not overflow, and
        // LLVM may build on that (decision 23.6).
        if self.release {
            let plain = match op { "sadd" => "add", "ssub" => "sub", _ => "mul" };
            let sure = if self.fast { " nsw" } else { "" };
            return self.set(format!("{plain}{sure} i64 {left}, {right}"));
        }
        let name = match op {
            "sadd" => "llvm.sadd.with.overflow.i64",
            "ssub" => "llvm.ssub.with.overflow.i64",
            _ => "llvm.smul.with.overflow.i64",
        };
        let both = self.call("{ i64, i1 }", name, format!("i64 {left}, i64 {right}"));
        let result = self.set(format!("extractvalue {{ i64, i1 }} {both}, 0"));
        let overflowed = self.set(format!("extractvalue {{ i64, i1 }} {both}, 1"));
        let n = self.number();
        let (overflow, fits) = (format!("overflow{n}"), format!("fits{n}"));
        self.jump_if(&overflowed, &overflow, &fits);
        self.place(&overflow);
        self.run("zeno_overflow", at(span));
        self.end("unreachable");
        self.place(&fits);
        result
    }

    /// A quantity turned into another unit: multiplied or divided by a
    /// number that is known now, while compiling.
    fn scaled(&mut self, code: &str, scale: units::Scale) -> String {
        if scale.times != 1.0 {
            self.set(format!("fmul double {code}, {}", float_code(scale.times)))
        } else if scale.over != 1.0 {
            self.set(format!("fdiv double {code}, {}", float_code(scale.over)))
        } else {
            code.to_string()
        }
    }

    /// Two quantities, both scaled to the unit they share: the finer of
    /// the two, so that `1 s + 500 ms` is `1500 ms`.
    fn in_common(&mut self, left: &Val, a: &str, right: &Val, b: &str, span: Span) -> Result<(String, String, String), Error> {
        match units::common(a, b) {
            Some((unit, l, r)) => Ok((unit.to_string(), self.scaled(&left.code, l), self.scaled(&right.code, r))),
            None => Err(cannot(format!("a quantity in {a} with a quantity in {b}"), span)),
        }
    }

    /// What `*` or `/` makes of two quantities, or of a number over one
    /// (decision 43): its kind, and each side scaled to the unit of its
    /// own kind with no prefix. `left` is `None` for a plain number.
    fn made(&mut self, op: &str, left: Option<(&Val, &str)>, right: (&Val, &str), span: Span) -> Result<(Kind, String, String), Error> {
        let refused = || cannot(format!("`{op}` between these units"), span);
        let made = units::made(op, left.map(|(_, unit)| unit), right.1).ok_or_else(refused)?;
        let l = match left {
            Some((val, unit)) => {
                let scale = units::to_plain(unit).ok_or_else(refused)?;
                self.scaled(&val.code, scale)
            }
            None => String::new(),
        };
        let scale = units::to_plain(right.1).ok_or_else(refused)?;
        let r = self.scaled(&right.0.code, scale);
        let kind = match made {
            units::Made::Unit(unit) => Kind::Quantity(unit.to_string()),
            units::Made::Number => Kind::Float,
        };
        Ok((kind, l, r))
    }

    fn arithmetic(&mut self, op: BinaryOp, left: Val, right: Val, span: Span) -> Result<Val, Error> {
        use BinaryOp::*;
        use Kind::{Float, Int, Quantity, Str};
        let (l, r) = (left.code.clone(), right.code.clone());
        let numbers = matches!(left.kind, Int | Float) && matches!(right.kind, Int | Float);

        Ok(match (op, &left.kind, &right.kind) {
            // `p + 4`: the address four items further on.
            (Add | Sub, Kind::Pointer(pointee), Int) => {
                let by = if op == Sub { self.set(format!("sub i64 0, {r}")) } else { r.clone() };
                let code = self.set(format!("getelementptr {}, ptr {l}, i64 {by}", pointee.ir_type()));
                Val::new(code, left.kind.clone())
            }
            (Add, Int, Int) => Val::new(self.checked("sadd", &l, &r, span), Int),
            (Sub, Int, Int) => Val::new(self.checked("ssub", &l, &r, span), Int),
            (Mul, Int, Int) => Val::new(self.checked("smul", &l, &r, span), Int),
            // These have more to them than one instruction (`//` rounds
            // down, `%` follows the right side's sign). `//` and `%` are
            // functions written in IR (see `WRITTEN`), `**` is in the
            // runtime.
            //
            // By 2, 4, 8 and so on, written as a number, they are one
            // instruction and cannot fail: a shift that keeps the sign
            // rounds down, and the low bits are what is left over
            // (decision 23.5).
            (FloorDiv, Int, Int) if emit_c::power_of_two(&r).is_some() => {
                let by = emit_c::power_of_two(&r).unwrap_or(0);
                Val::new(self.set(format!("ashr i64 {l}, {by}")), Int)
            }
            (Mod, Int, Int) if emit_c::power_of_two(&r).is_some() => {
                let low = (1u64 << emit_c::power_of_two(&r).unwrap_or(0)) - 1;
                Val::new(self.set(format!("and i64 {l}, {low}")), Int)
            }
            (FloorDiv | Mod | Pow, Int, Int) => {
                let name = match op {
                    FloorDiv => "zeno.floordiv",
                    Mod => "zeno.mod",
                    _ => "zeno_pow",
                };
                Val::new(self.call("i64", name, format!("i64 {l}, i64 {r}, {}", at(span))), Int)
            }

            // The bits of whole numbers: one instruction each.
            (BitAnd | BitOr | BitXor, Int, Int) => {
                let instruction = match op {
                    BitAnd => "and",
                    BitOr => "or",
                    _ => "xor",
                };
                Val::new(self.set(format!("{instruction} i64 {l}, {r}")), Int)
            }
            // The two shifts go through the runtime, which checks how far.
            (Shl | Shr, Int, Int) => {
                let name = if op == Shl { "zeno_shl" } else { "zeno_shr" };
                Val::new(self.call("i64", name, format!("i64 {l}, i64 {r}, {}", at(span))), Int)
            }

            (Add, Str, Str) => Val::new(self.join(&[l, r]), Str),
            // `bytes + [300]`: a list of bytes and a list of ints make a
            // list of ints (36.2).
            (Add, Kind::List(a), Kind::List(b))
                if matches!((&**a, &**b), (Kind::Byte, Kind::Int) | (Kind::Int, Kind::Byte)) =>
            {
                let (a_bytes, b_bytes) = (**a == Kind::Byte, **b == Kind::Byte);
                let joined = self.call("ptr", "zeno_list_concat_mixed", format!("ptr {l}, i1 {a_bytes}, ptr {r}, i1 {b_bytes}"));
                Val::new(joined, Kind::List(Box::new(Int))).made()
            }
            // `[0] * 64`: a new list with the items so many times over.
            (Mul, Kind::List(a), Int) => {
                let repeat = if **a == Kind::Byte { "zeno_bytes_repeat" } else { "zeno_list_repeat" };
                let mut repeated = self.call("ptr", repeat, format!("ptr {l}, i64 {r}, {}", at(span)));
                if self.keeps_copies(a) {
                    repeated = self.keep_items(a, &repeated, span)?;
                }
                Val::new(repeated, left.kind.clone()).made()
            }
            // A new list with the items of both.
            (Add, Kind::List(a), Kind::List(b)) if a == b => {
                let mut joined = self.call("ptr", list_fn(a, "concat"), format!("ptr {l}, ptr {r}"));
                if self.keeps_copies(a) {
                    // With no collector a list of text has its own copy
                    // of each item, and so has the list that `+` makes.
                    joined = self.keep_items(a, &joined, span)?;
                }
                Val::new(joined, left.kind.clone()).made()
            }

            // Quantities. The checker has made sure the two measure the
            // same thing. If the units differ, both sides are scaled to
            // the finer one.
            (Add | Sub, Quantity(a), Quantity(b)) => {
                let (unit, l, r) = self.in_common(&left, a, &right, b, span)?;
                let instruction = if op == Add { "fadd" } else { "fsub" };
                Val::new(self.set(format!("{instruction} double {l}, {r}")), Quantity(unit))
            }
            (Mul, Quantity(unit), Int | Float) | (Mul, Int | Float, Quantity(unit)) => {
                let (l, r) = (self.double(&left), self.double(&right));
                Val::new(self.set(format!("fmul double {l}, {r}")), Quantity(unit.clone()))
            }
            // `3.3 V * 20 mA` and `1 / (2 ms)` (decision 43): each side in
            // the unit of its kind with no prefix, as the interpreter does.
            (Mul, Quantity(a), Quantity(b)) => {
                let (kind, l, r) = self.made("*", Some((&left, a)), (&right, b), span)?;
                Val::new(self.set(format!("fmul double {l}, {r}")), kind)
            }
            (Div, Quantity(a), Quantity(b)) if units::common(a, b).is_none() => {
                let (kind, l, r) = self.made("/", Some((&left, a)), (&right, b), span)?;
                Val::new(self.call("double", "zeno.div", format!("double {l}, double {r}, {}", at(span))), kind)
            }
            (Div, Int | Float, Quantity(b)) => {
                let l = self.double(&left);
                let (kind, _, r) = self.made("/", None, (&right, b), span)?;
                Val::new(self.call("double", "zeno.div", format!("double {l}, double {r}, {}", at(span))), kind)
            }
            // Ten seconds over two seconds is five, with no unit left.
            (Div, Quantity(a), Quantity(b)) => {
                let (_, l, r) = self.in_common(&left, a, &right, b, span)?;
                Val::new(self.call("double", "zeno.div", format!("double {l}, double {r}, {}", at(span))), Float)
            }
            (Div, Quantity(unit), Int | Float) => {
                let r = self.double(&right);
                let args = format!("double {l}, double {r}, {}", at(span));
                Val::new(self.call("double", "zeno.div", args), Quantity(unit.clone()))
            }

            // Two u64s (decision 42). The checker has made sure both
            // sides are u64s. `--release` wraps round, as it does for an
            // int; otherwise the runtime checks, as in the C backend.
            (Add | Sub | Mul, Kind::U64, Kind::U64) if self.release => {
                let plain = match op { Add => "add", Sub => "sub", _ => "mul" };
                Val::new(self.set(format!("{plain} i64 {l}, {r}")), Kind::U64)
            }
            (Add | Sub | Mul | FloorDiv | Mod | Pow, Kind::U64, Kind::U64) => {
                let name = match op {
                    Add => "zeno_add_u64",
                    Sub => "zeno_sub_u64",
                    Mul => "zeno_mul_u64",
                    FloorDiv => "zeno_floordiv_u64",
                    Mod => "zeno_mod_u64",
                    _ => "zeno_pow_u64",
                };
                Val::new(self.call("i64", name, format!("i64 {l}, i64 {r}, {}", at(span))), Kind::U64)
            }
            (Div, Kind::U64, Kind::U64) => {
                let (l, r) = (self.double(&left), self.double(&right));
                Val::new(self.call("double", "zeno.div", format!("double {l}, double {r}, {}", at(span))), Float)
            }
            (BitAnd | BitOr | BitXor, Kind::U64, Kind::U64) => {
                let instruction = match op {
                    BitAnd => "and",
                    BitOr => "or",
                    _ => "xor",
                };
                Val::new(self.set(format!("{instruction} i64 {l}, {r}")), Kind::U64)
            }
            (Shl | Shr, Kind::U64, Int | Kind::U64) => {
                let name = if op == Shl { "zeno_shl_u64" } else { "zeno_shr_u64" };
                let args = format!("i64 {l}, i64 {r}, i1 {}, {}", right.kind == Int, at(span));
                Val::new(self.call("i64", name, args), Kind::U64)
            }

            // An f32 with an f32 or an int (decision 41.2), worked out in
            // double and rounded to 32 bits, which gives the f32 answer.
            (_, Kind::F32, Kind::F32 | Int) | (_, Int, Kind::F32) => {
                let (l, r) = (self.single(&left), self.single(&right));
                let wide = match op {
                    Add => self.set(format!("fadd double {l}, {r}")),
                    Sub => self.set(format!("fsub double {l}, {r}")),
                    Mul => self.set(format!("fmul double {l}, {r}")),
                    Div => self.call("double", "zeno.div", format!("double {l}, double {r}, {}", at(span))),
                    FloorDiv => self.call("double", "zeno_floordiv_float", format!("double {l}, double {r}, {}", at(span))),
                    Mod => self.call("double", "zeno_mod_float", format!("double {l}, double {r}, {}", at(span))),
                    Pow => self.call("double", "pow", format!("double {l}, double {r}")),
                    _ => {
                        return Err(cannot(
                            format!("`{}` between {} and {}", op.text(), described(&left.kind), described(&right.kind)),
                            span,
                        ))
                    }
                };
                Val::new(self.rounded(&wide), Kind::F32)
            }

            // What is left is floats, or an int with a float. The float
            // instructions already do what Zeno wants, except for
            // dividing by zero.
            (Add | Sub | Mul, _, _) if numbers => {
                let (l, r) = (self.double(&left), self.double(&right));
                let instruction = match op {
                    Add => "fadd",
                    Sub => "fsub",
                    _ => "fmul",
                };
                Val::new(self.set(format!("{instruction} double {l}, {r}")), Float)
            }
            (Div | FloorDiv | Mod, _, _) if numbers => {
                let (l, r) = (self.double(&left), self.double(&right));
                let name = match op {
                    Div => "zeno.div",
                    FloorDiv => "zeno_floordiv_float",
                    _ => "zeno_mod_float",
                };
                Val::new(self.call("double", name, format!("double {l}, double {r}, {}", at(span))), Float)
            }
            (Pow, _, _) if numbers => {
                let (l, r) = (self.double(&left), self.double(&right));
                Val::new(self.call("double", "pow", format!("double {l}, double {r}")), Float)
            }

            _ => {
                return Err(cannot(
                    format!("`{}` between {} and {}", op.text(), described(&left.kind), described(&right.kind)),
                    span,
                ))
            }
        })
    }

    /// `a and b`, `a or b`. The right side must not run unless it is needed.
    ///
    /// ```text
    ///   br i1 %a, label %and1, label %end1     ; a is false: skip b
    /// and1:
    ///   ...work out b...
    ///   br label %end1
    /// end1:
    ///   %t = phi i1 [ false, %entry ], [ %b, %and1 ]
    /// ```
    ///
    /// `phi` is how a value that depends on the way a block was reached
    /// is written when names can be given a value only once: "false if we
    /// came from `entry`, `%b` if we came from `and1`".
    fn logic(&mut self, op: BinaryOp, left: &Expr, right: &Expr) -> Result<Val, Error> {
        let is_and = op == BinaryOp::And;
        let first = self.condition(left)?;
        let from = self.code.block.clone();
        let n = self.number();
        let (more, end) = (format!("{}{n}", if is_and { "and" } else { "or" }), format!("end{n}"));
        match is_and {
            true => self.jump_if(&first, &more, &end),
            false => self.jump_if(&first, &end, &more),
        }
        self.place(&more);
        let second = self.condition(right)?;
        // Working out the right side may have ended in another block.
        let after = self.code.block.clone();
        self.place(&end);
        let settled = if is_and { "false" } else { "true" };
        let result = self.set(format!("phi i1 [ {settled}, %{from} ], [ {second}, %{after} ]"));
        Ok(Val::new(result, Kind::Bool))
    }

    /// `a < b`, and `a < b < c`: each piece is worked out once, and only
    /// if the comparisons before it held.
    fn chain(&mut self, first: &Expr, rest: &[(BinaryOp, Expr)]) -> Result<Val, Error> {
        let mut previous = self.expr(first)?;
        let n = if rest.len() > 1 { self.number() } else { 0 };
        let end = format!("end{n}");
        // The blocks that leave early because a comparison failed.
        let mut failed = Vec::new();
        let mut held = String::new();
        for (index, (op, expr)) in rest.iter().enumerate() {
            let next = self.expr(expr)?;
            held = self.compare(*op, &previous, &next, expr.span)?;
            if index + 1 < rest.len() {
                failed.push(self.code.block.clone());
                let more = format!("then{n}.{}", index + 1);
                self.jump_if(&held, &more, &end);
                self.place(&more);
            }
            previous = next;
        }
        // One comparison needs no blocks at all.
        if failed.is_empty() {
            return Ok(Val::new(held, Kind::Bool));
        }
        let last = self.code.block.clone();
        self.place(&end);
        let mut ways: Vec<String> = failed.iter().map(|block| format!("[ false, %{block} ]")).collect();
        ways.push(format!("[ {held}, %{last} ]"));
        Ok(Val::new(self.set(format!("phi i1 {}", ways.join(", "))), Kind::Bool))
    }

    /// `==` and the rest, between two values.
    fn compare(&mut self, op: BinaryOp, left: &Val, right: &Val, span: Span) -> Result<String, Error> {
        let plain =
            |kind: &Kind| matches!(kind, Kind::Int | Kind::Float | Kind::F32 | Kind::U64 | Kind::Bool | Kind::Str | Kind::Quantity(_));
        if plain(&left.kind) && plain(&right.kind) || !matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
            return self.ordered(op, left, right, span);
        }
        // A list of bytes and a list of ints are equal when their
        // numbers are (36.2).
        let ints = Kind::List(Box::new(Kind::Int));
        let bytes = Kind::List(Box::new(Kind::Byte));
        let mixed = match (&left.kind, &right.kind) {
            (a, b) if *a == bytes && *b == ints => Some((left.code.clone(), right.code.clone())),
            (a, b) if *a == ints && *b == bytes => Some((right.code.clone(), left.code.clone())),
            _ => None,
        };
        if let Some((bytes, ints)) = mixed {
            let same = self.call("i1", "zeno_bytes_eq_ints", format!("ptr {bytes}, ptr {ints}"));
            return Ok(if op == BinaryOp::Eq { same } else { self.set(format!("xor i1 {same}, true")) });
        }
        // Everything else is equal when its contents are. `x == none` and
        // `x == 5`, where `x` may be `none`, first bring both sides to
        // the same kind.
        let Some(kind) = joined(&left.kind, &right.kind) else {
            return Err(cannot(
                format!("`{}` between {} and {}", op.text(), described(&left.kind), described(&right.kind)),
                span,
            ));
        };
        let left = self.coerce(left.clone(), &kind, span)?;
        let right = self.coerce(right.clone(), &kind, span)?;
        let same = self.equal(&kind, &left.code, &right.code, span)?;
        Ok(if op == BinaryOp::Eq { same } else { self.set(format!("xor i1 {same}, true")) })
    }

    /// One comparison between two numbers, bools, pieces of text or
    /// quantities.
    fn ordered(&mut self, op: BinaryOp, left: &Val, right: &Val, span: Span) -> Result<String, Error> {
        use BinaryOp::*;
        use Kind::{Bool, Float, Int, Quantity, Str};
        // `icmp` compares whole numbers, and wants to be told that they
        // are signed (`slt` is "signed less than"). `fcmp` compares
        // floats. Its `o` is for "ordered": false if either side is not
        // a number at all. `!=` is the one that is true then.
        let (whole, float) = match op {
            Eq => ("eq", "oeq"),
            NotEq => ("ne", "une"),
            Lt => ("slt", "olt"),
            LtEq => ("sle", "ole"),
            Gt => ("sgt", "ogt"),
            GtEq => ("sge", "oge"),
            _ => return Err(cannot(format!("`{}`", op.text()), span)),
        };
        Ok(match (&left.kind, &right.kind) {
            (Int, Int) => self.set(format!("icmp {whole} i64 {}, {}", left.code, right.code)),
            // Two u64s: the unsigned comparisons, `ult` for "unsigned
            // less than" where an int has `slt`.
            (Kind::U64, Kind::U64) => {
                let unsigned = whole.replacen('s', "u", 1);
                self.set(format!("icmp {unsigned} i64 {}, {}", left.code, right.code))
            }
            (Bool, Bool) if matches!(op, Eq | NotEq) => {
                self.set(format!("icmp {whole} i1 {}, {}", left.code, right.code))
            }
            (Int | Float, Int | Float) => {
                let (l, r) = (self.double(left), self.double(right));
                self.set(format!("fcmp {float} double {l}, {r}"))
            }
            (Kind::F32, Kind::F32 | Int) | (Int, Kind::F32) => {
                let (l, r) = (self.single(left), self.single(right));
                self.set(format!("fcmp {float} double {l}, {r}"))
            }
            (Quantity(a), Quantity(b)) => {
                let (_, l, r) = self.in_common(left, a, right, b, span)?;
                self.set(format!("fcmp {float} double {l}, {r}"))
            }
            // Two pieces of text are compared by what they say, which
            // `strcmp` answers with a number below, at or above zero.
            (Str, Str) => {
                let order = self.call("i32", "strcmp", format!("ptr {}, ptr {}", left.code, right.code));
                self.set(format!("icmp {whole} i32 {order}, 0"))
            }
            _ => {
                return Err(cannot(
                    format!("`{}` between {} and {}", op.text(), described(&left.kind), described(&right.kind)),
                    span,
                ))
            }
        })
    }

    /// `x in items`, `x not in items`, and the same for a piece of a str.
    fn contains(&mut self, op: BinaryOp, left: Val, right: Val, span: Span) -> Result<Val, Error> {
        let found = match (&left.kind, &right.kind) {
            (Kind::Int, Kind::List(items)) if **items == Kind::Byte => {
                self.call("i1", "zeno_bytes_contains", format!("ptr {}, i64 {}", right.code, left.code))
            }
            // `none in items` and `2 in items`, where the items may be
            // `none`, first become what the list holds.
            (item, Kind::List(items)) if joined(item, items).as_ref() == Some(&**items) => {
                let same = self.eq_adapter(items, span)?;
                let left = self.coerce(left.clone(), items, span)?;
                let slot = self.looked_for(&left, span)?;
                self.call("i1", "zeno_list_contains", format!("ptr {}, i64 {slot}, ptr {same}", right.code))
            }
            // `"Ada" in ages`: is there such a key? The runtime gives
            // where it is, or -1.
            (item, Kind::Dict(key, _)) if item == &**key => {
                let slot = self.to_slot(&left, span)?;
                let at = self.call("i64", "zeno_dict_find", format!("ptr {}, i64 {slot}", right.code));
                self.set(format!("icmp sge i64 {at}, 0"))
            }
            // `strstr` gives the address where the piece was found, or
            // null.
            (Kind::Str, Kind::Str) => {
                let found = self.call("ptr", "strstr", format!("ptr {}, ptr {}", right.code, left.code));
                self.set(format!("icmp ne ptr {found}, null"))
            }
            _ => {
                return Err(cannot(
                    format!("`in` between {} and {}", described(&left.kind), described(&right.kind)),
                    span,
                ))
            }
        };
        let code = if op == BinaryOp::NotIn { self.set(format!("xor i1 {found}, true")) } else { found };
        Ok(Val::new(code, Kind::Bool))
    }

    /// `object.name`: a field of an object, or a quantity in another unit.
    /// `object.name`. A number in an object a parameter holds may be read
    /// once, in the function's entry block, and the value used from then
    /// on: see `ast::fields_read_once` for when.
    fn field(&mut self, object: &Expr, name: &str, span: Span) -> Result<Val, Error> {
        let key = match (&object.kind, &self.once) {
            (ExprKind::Name(param), Some((params, written, _)))
                if params.contains(param)
                    && !written.contains(name)
                    && self.lookup(param).is_some_and(|variable| matches!(variable.place, Place::Value(_))) =>
            {
                (param.clone(), name.to_string())
            }
            _ => return self.field_read(object, name, span),
        };
        if let Some(val) = self.once.as_ref().and_then(|(_, _, made)| made.get(&key)) {
            return Ok(val.clone());
        }
        let before = self.code.lines.len();
        let val = self.field_read(object, name, span)?;
        let number = matches!(val.kind, Kind::Int | Kind::Float | Kind::Bool);
        let plain = self.code.lines[before..].iter().all(|line| {
            let line = line.trim_start();
            line.starts_with("%t") && (line.contains("= getelementptr ") || line.contains("= load ") || line.contains("= sext ") || line.contains("= zext "))
        });
        if number && plain {
            let moved: Vec<String> = self.code.lines.drain(before..).collect();
            self.code.slots.extend(moved);
            if let Some((_, _, made)) = self.once.as_mut() {
                made.insert(key, val.clone());
            }
        }
        Ok(val)
    }

    fn field_read(&mut self, object: &Expr, name: &str, span: Span) -> Result<Val, Error> {
        if let ExprKind::Name(owner) = &object.kind {
            if self.lookup(owner).is_none() {
                if let ("math", Some(number)) = (owner.as_str(), emit_c::math_constant(name)) {
                    return Ok(Val::new(float_code(number), Kind::Float));
                }
                if owner == "board" && name == "led" {
                    return Ok(Val::new(LED.to_string(), Kind::Pin));
                }
                if owner == "board" && matches!(name, "pull_up" | "pull_down" | "rising" | "falling" | "either") {
                    return Ok(Val::new(self.string(name), Kind::Str).fixed());
                }
                // `Shape.Idle`
                if self.enums.contains_key(owner) && self.cases.get(name).is_some_and(|(of, _)| of == owner) {
                    let index = self.cases[name].1;
                    if self.enums[owner].cases[index].1.is_empty() {
                        return Ok(Val::new(bare_case(owner, name), Kind::Enum(owner.clone())));
                    }
                }
            }
        }
        let val = self.expr(object)?;
        match &val.kind {
            // `(3 s).ms`: the same quantity in another unit.
            Kind::Quantity(unit) if units::is_unit(name) => match units::convert(unit, name) {
                Some(scale) => Ok(Val::new(self.scaled(&val.code, scale), Kind::Quantity(name.to_string()))),
                None => Err(cannot(format!("`.{name}` on this"), span)),
            },
            // Where the field is, then what is there.
            Kind::Class(class) => match self.field_of(class, name) {
                Some((_, index, kind)) => {
                    // A list or an object inside an object that the
                    // statement owns is gone when the statement is.
                    let passing = val.passing && matches!(kind, Kind::List(_) | Kind::Dict(..) | Kind::Class(_));
                    let val = Val::new(self.load_field(class, &val.code, index, span)?, kind);
                    Ok(Val { passing, ..self.lend(val) })
                }
                None => Err(Error::new(
                    format!(
                        "`zeno build --llvm` cannot compile a method used as a value of an object that no variable holds, \
                         `.{name}`, yet. Put the object in a `let` first and use `.{name}` of that: then it builds. \
                         `zeno run` can still run this program"
                    ),
                    span,
                )),
            },
            Kind::Error if matches!(name, "message" | "kind") => {
                let at = self.field_at(ERROR, &val.code, if name == "kind" { 0 } else { 1 });
                Ok(Val::new(self.set(format!("load ptr, ptr {at}")), Kind::Str))
            }
            // `led.number`: a pin is its number already.
            Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Interrupt if name == "number" => {
                Ok(Val { kind: Kind::Int, ..val })
            }
            Kind::AnalogPin if name == "max_count" => Ok(Val::new(self.call("i64", "zeno_analog_max", ""), Kind::Int)),
            Kind::Ticker if name == "period" => {
                let period = self.call("double", "zeno_ticker_period", format!("i64 {}", val.code));
                Ok(Val::new(period, Kind::Quantity("ms".to_string())))
            }
            Kind::Register(_) if name == "address" => Ok(Val { kind: Kind::Int, ..val }),
            // The checker took it, so it is a method of a list, a text or
            // a dict, which is written in place where it is called.
            Kind::List(_) | Kind::Str | Kind::Dict(..) => {
                Err(cannot(format!("a method that comes with Zeno used as a value, `.{name}`,"), span))
            }
            _ => Err(cannot(format!("`.{name}` on {}", described(&val.kind)), span)),
        }
    }

    // ---------------------------------------------------------------- Lists

    /// A value on its way into a list. Every item of a list is kept in a
    /// slot of 64 bits, whatever it is, so that one runtime serves lists
    /// of everything. A whole number goes in as it is; anything else is
    /// carried over bit for bit: `bitcast` reads the 64 bits of a float
    /// as a whole number, `ptrtoint` does the same for an address.
    fn to_slot(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        Ok(match &val.kind {
            Kind::Int | Kind::Byte | Kind::U64 | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => val.code.clone(),
            Kind::Float | Kind::F32 | Kind::Quantity(_) => self.set(format!("bitcast double {} to i64", val.code)),
            // "Zero extend": the one bit, with 63 zeros in front.
            Kind::Bool => self.set(format!("zext i1 {} to i64", val.code)),
            kind if is_pointer(kind) => self.set(format!("ptrtoint ptr {} to i64", val.code)),
            Kind::Optional(inner) if is_pointer(inner) => self.set(format!("ptrtoint ptr {} to i64", val.code)),
            // A number and its flag do not fit in a slot; see `zeno.box.int`.
            Kind::Optional(inner) => {
                let name = match scalar(inner, span)? {
                    "int" => "zeno.box.int",
                    "float" => "zeno.box.float",
                    _ => "zeno.box.bool",
                };
                let boxed = self.call("ptr", name, format!("{} {}", ir_type(&val.kind, span)?, val.code));
                self.set(format!("ptrtoint ptr {boxed} to i64"))
            }
            // `[]` or `[none]` where nothing says what it holds.
            Kind::None | Kind::Other(_) => return Err(emit_c::untyped_list("`zeno build --llvm`", span)),
            _ => return Err(cannot("a list of these", span)),
        })
    }

    /// And out of a list again.
    fn from_slot(&mut self, kind: &Kind, slot: &str, span: Span) -> Result<Val, Error> {
        let code = match kind {
            Kind::Byte => return Ok(Val::new(slot.to_string(), Kind::Int)),
            Kind::Int | Kind::U64 | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => slot.to_string(),
            Kind::Float | Kind::F32 | Kind::Quantity(_) => self.set(format!("bitcast i64 {slot} to double")),
            // "Truncate": keep the lowest bit.
            Kind::Bool => self.set(format!("trunc i64 {slot} to i1")),
            kind if is_pointer(kind) => self.set(format!("inttoptr i64 {slot} to ptr")),
            Kind::Optional(inner) if is_pointer(inner) => self.set(format!("inttoptr i64 {slot} to ptr")),
            Kind::Optional(inner) => {
                let name = match scalar(inner, span)? {
                    "int" => "zeno.unbox.int",
                    "float" => "zeno.unbox.float",
                    _ => "zeno.unbox.bool",
                };
                let boxed = self.set(format!("inttoptr i64 {slot} to ptr"));
                self.call(ir_type(kind, span)?, name, format!("ptr {boxed}"))
            }
            Kind::None | Kind::Other(_) => return Err(emit_c::untyped_list("`zeno build --llvm`", span)),
            _ => return Err(cannot("a list of these", span)),
        };
        Ok(Val::new(code, kind.clone()))
    }

    /// `[1, 2, 3]`
    fn list(&mut self, expr: &Expr, items: &[Expr], span: Span) -> Result<Val, Error> {
        let mut vals = Vec::new();
        let mut held = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let val = self.expr(item)?;
            self.stays(&val, item.span)?;
            // An item that has an owner of its own has left that owner
            // and not yet reached the list. In a firmware build a later
            // item may fail, and the failure leaves this function at
            // once. Hold the item until the list is made, so that such
            // a failure frees it.
            if self.firmware && self.drops.owns(&val.kind) && index + 1 < items.len() {
                held.push(self.hold(val.kind.clone(), &val.code));
            }
            vals.push(val);
        }
        for key in &held {
            self.let_go(key);
        }
        let Some(first) = vals.first() else {
            // `[]` alone does not say what it will hold. The place it
            // arrives at does (`coerce`), and a `let` with no type
            // beside it says it cannot tell.
            let kind = Kind::List(Box::new(Kind::Other("nothing yet".to_string())));
            return Ok(Val::new(self.call("ptr", "zeno_list_new", "i64 0"), kind));
        };
        // `[1, none]` is a list of `int or none`: find the one kind that
        // every item can be stored as.
        let mut item = Some(first.kind.clone());
        for val in &vals {
            item = item.and_then(|item| joined(&item, &val.kind));
        }
        // Where a list of an interface is wanted, the checker has said
        // so: the items are of different classes.
        if let Some(wanted) = self.types.items(expr) {
            item = Some(shared(wanted));
        }
        // A list[u8] keeps one byte to an item (decision 36.2).
        if self.types.makes_bytes(expr) {
            item = Some(Kind::Byte);
        }
        let Some(item) = item else {
            return Err(cannot("a list whose items are not all one type", span));
        };
        // `zeno_list_of` takes a count and then that many slots.
        let mut slots = format!("i32 {}", vals.len());
        for (mut val, written) in vals.into_iter().zip(items) {
            // Every item has been worked out by now, so each is copied
            // from what the statement holds.
            val.lent = None;
            let val = self.keep(val, is_written(written));
            let val = self.coerce(val, &item, span)?;
            slots += &format!(", i64 {}", self.kept_slot(&val, span)?);
        }
        let of = if item == Kind::Byte { "zeno_bytes_of" } else { "zeno_list_of" };
        self.uses(of);
        let list = self.set(format!("call ptr (i32, ...) @{of}({slots})"));
        Ok(Val::new(list, Kind::List(Box::new(item))))
    }

    /// `["Ada": 36, "Alan": 41]`: a new dict, and then one call for each
    /// entry, left to right.
    ///
    /// ```text
    ///   %t1 = call ptr @zeno_dict_new(i32 115, i32 105)       ; 's' and 'i'
    ///   %t2 = ptrtoint ptr @.str.1 to i64
    ///   call void @zeno_dict_set(ptr %t1, i64 %t2, i64 36)
    /// ```
    fn dict(&mut self, expr: &Expr, entries: &[(Expr, Expr)], span: Span) -> Result<Val, Error> {
        let mut vals = Vec::new();
        for (key, value) in entries {
            let key = self.expr(key)?;
            let value = self.expr(value)?;
            vals.push((key, value));
        }
        let Some((first_key, first_value)) = vals.first() else {
            // `[:]` alone does not say what it will hold. The place it
            // arrives at does (`coerce`).
            let nothing_yet = || Box::new(Kind::Other("nothing yet".to_string()));
            let dict = self.call("ptr", "zeno_dict_new", "i32 0, i32 0");
            return Ok(Val::new(dict, Kind::Dict(nothing_yet(), nothing_yet())));
        };
        // The values are all of one kind. Where the place says more
        // than they do (a `dict[str, Shape]`), the checker has said what
        // that kind is.
        let key = first_key.kind.clone();
        let mut value = Some(first_value.kind.clone());
        for (_, a_value) in &vals {
            value = value.and_then(|value| joined(&value, &a_value.kind));
        }
        if let Some(wanted) = self.types.items(expr) {
            value = Some(shared(wanted));
        }
        let Some(value) = value else {
            return Err(cannot("a dict whose values are not all one type", span));
        };
        self.fits_a_slot(&value, span)?;
        let kinds = self.dict_kinds(&key, &value, span)?;
        if vals.iter().any(|(a_key, _)| a_key.kind != key) {
            return Err(cannot("a dict whose keys are not all one type", span));
        }
        let dict = self.call("ptr", "zeno_dict_new", kinds);
        for ((a_key, mut a_value), (_, written)) in vals.into_iter().zip(entries) {
            // The dict keeps a copy of its own of each value that is
            // text, as a list does of each item. Every value has been
            // worked out by now, so each is copied from what the
            // statement holds.
            a_value.lent = None;
            let a_value = self.keep(a_value, is_written(written));
            let a_value = self.coerce(a_value, &value, span)?;
            let key_slot = self.to_slot(&a_key, span)?;
            let value_slot = self.to_slot(&a_value, span)?;
            self.run("zeno_dict_set", format!("ptr {dict}, i64 {key_slot}, i64 {value_slot}"));
        }
        Ok(Val::new(dict, Kind::Dict(Box::new(key), Box::new(value))))
    }

    /// `[n * n for n in items if n > 0]` is a loop that fills a list.
    fn comprehension(
        &mut self,
        element: &Expr,
        variable: &str,
        iterable: &Expr,
        condition: Option<&Expr>,
        bytes: bool,
        span: Span,
    ) -> Result<Val, Error> {
        // A list of `u8`s is a list of bytes (36.2).
        let list = self.call("ptr", if bytes { "zeno_bytes_new" } else { "zeno_list_new" }, "i64 0");
        // Until it is full, the list is nobody's. If working out an item
        // fails, the failure frees what there is of it.
        let filling = self.hold(Kind::List(Box::new(Kind::None)), &list);
        let filled_from = self.code.lines.len();
        let count = self.open_loop(variable, None, iterable, None)?;
        // Text built for one item is given back before the next.
        let begin = self.text_begin();
        if let Some(condition) = condition {
            // An item that fails the test is skipped: on to the next trip.
            let wanted = self.condition(condition)?;
            let keep = format!("keep{}", self.number());
            self.jump_if(&wanted, &keep, &count.next);
            self.place(&keep);
        }
        // So is what working out one item makes and gives to nobody.
        let own = self.own_begin();
        let item = self.expr(element)?;
        self.stays(&item, element.span)?;
        let item = self.keep(item, is_written(element));
        let owns = self.drops.owns(&item.kind);
        if self.keeps_copies(&item.kind) || owns {
            // The list has copies of its items, or is the owner of them,
            // which was not known when the lines above were written: a
            // failure half way has to free those as well.
            let whole = match owns {
                true => self.drop_list(&item.kind, span)?,
                false if is_boxed(&item.kind) => {
                    self.uses("zeno.list.drop.boxes");
                    "@zeno.list.drop.boxes".to_string()
                }
                false => {
                    self.uses("zeno_list_drop_text");
                    "@zeno_list_drop_text".to_string()
                }
            };
            let (plain, whole) = (format!("call void @zeno_list_drop(ptr {list})"), format!("call void {whole}(ptr {list})"));
            for line in &mut self.code.lines[filled_from..] {
                if line.trim_start() == plain {
                    *line = line.replace(&plain, &whole);
                }
            }
        }
        let item = if bytes { self.coerce(item, &Kind::Byte, span)? } else { item };
        let slot = self.kept_slot(&item, span)?;
        self.run(list_fn(&item.kind, "append"), format!("ptr {list}, i64 {slot}"));
        self.own_end(own, true)?;
        if let Some(mark) = self.text_end(begin) {
            self.text_back(&mark);
        }
        self.close_loop(&count);
        self.let_go(&filling);
        Ok(Val::new(list, Kind::List(Box::new(item.kind))))
    }

    // ------------------------------------------------ Values that may be none

    /// Make `val` fit a place that holds values of kind `to`.
    ///
    /// Nearly always there is nothing to do. The work is at the border of
    /// `T or none`: a plain number going into an `int or none` has a flag
    /// put beside it, and `none` becomes whatever "nothing" is for that
    /// kind.
    fn coerce(&mut self, val: Val, to: &Kind, span: Span) -> Result<Val, Error> {
        if val.kind == *to {
            return Ok(val);
        }
        // A Circle arriving where a Shape is wanted: the same address,
        // seen as a `Shape` from here on.
        if let (Kind::Class(from), Kind::Class(interface)) = (emit_c::some(&val.kind), emit_c::some(to)) {
            let is_one = self.is_a.get(interface).is_some_and(|classes| classes.iter().any(|(_, class)| class == from));
            if from != interface && is_one {
                return Ok(Val { kind: to.clone(), ..val });
            }
        }
        // An empty list becomes a list of whatever this place holds.
        let mut val = val;
        if is_empty_list(&val.kind) {
            match to {
                Kind::List(_) => return Ok(Val { kind: to.clone(), ..val }),
                Kind::Optional(inner) if matches!(**inner, Kind::List(_)) => val.kind = (**inner).clone(),
                _ => {}
            }
        }
        // `[:]` becomes a dict of whatever this place holds, and is told so.
        if is_empty_dict(&val.kind) {
            let wanted = match to {
                Kind::Optional(inner) => &**inner,
                to => to,
            };
            if let Kind::Dict(key, value) = wanted {
                let kinds = self.dict_kinds(key, value, span)?;
                self.call("ptr", "zeno_dict_kinds", format!("ptr {}, {kinds}", val.code));
                val.kind = wanted.clone();
                if val.kind == *to {
                    return Ok(val);
                }
            }
        }
        let code = match (&val.kind, to) {
            (Kind::None, Kind::Optional(inner)) => nothing(inner, span)?.to_string(),
            // An address already has a way to say "nothing".
            (from, Kind::Optional(inner)) if from == &**inner && is_pointer(from) => val.code.clone(),
            // `insertvalue` makes a copy of a pair with one part
            // replaced. The pair it starts from has the flag set and
            // nothing yet (`undef`) in the place of the number.
            (from, Kind::Optional(inner)) if from == &**inner => {
                let (pair, inside) = (ir_type(to, span)?, ir_type(from, span)?);
                self.set(format!("insertvalue {pair} {{ i1 true, {inside} undef }}, {inside} {}, 1", val.code))
            }
            (Kind::Function(from), Kind::Function(wanted)) if self.raising(from) != self.raising(wanted) => {
                return Err(cannot("a function value that raises where one that does not is wanted, or the other way round", span));
            }
            (Kind::Function(_), Kind::Function(_)) => match joined(&val.kind, to) {
                Some(_) => val.code.clone(),
                None => {
                    return Err(cannot("a function used where a function of a different shape is wanted", span))
                }
            },
            // A number going into a list of bytes: the checker has seen
            // that it fits (36.2).
            (Kind::Int, Kind::Byte) => val.code.clone(),
            (Kind::List(from), Kind::List(item)) if (**from == Kind::Byte) != (**item == Kind::Byte) => {
                return Err(cannot("a list of bytes where another list is wanted, or the other way round", span));
            }
            // Anything else the checker has already agreed to as it is.
            _ => return Ok(val),
        };
        Ok(Val { code, kind: to.clone(), lent: None, ..val })
    }

    /// `coerce`, and then make sure the value can be held where it is
    /// going. The checker has agreed to the types. This is the last look
    /// before IR is written that LLVM would refuse.
    fn fit(&mut self, val: Val, to: &Kind, span: Span) -> Result<Val, Error> {
        let val = self.coerce(val, to, span)?;
        if ir_type(&val.kind, span)? != ir_type(to, span)? {
            return Err(cannot(format!("{} where {} is wanted", described(&val.kind), described(to)), span));
        }
        Ok(val)
    }

    /// Is a value that may be `none` there? One bit.
    fn is_there(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        match &val.kind {
            Kind::Optional(inner) if is_pointer(inner) => Ok(self.set(format!("icmp ne ptr {}, null", val.code))),
            // `extractvalue` reads one part of a pair: part 0 is the flag.
            kind => Ok(self.set(format!("extractvalue {} {}, 0", ir_type(kind, span)?, val.code))),
        }
    }

    /// The value inside one that is known to be there.
    fn inside(&mut self, val: &Val, span: Span) -> Result<Val, Error> {
        let Kind::Optional(inner) = &val.kind else {
            return Ok(val.clone());
        };
        let code = match is_pointer(inner) {
            true => val.code.clone(),
            false => self.set(format!("extractvalue {} {}, 1", ir_type(&val.kind, span)?, val.code)),
        };
        Ok(Val { code, kind: (**inner).clone(), lent: None, ..val.clone() })
    }

    /// A variable has just been read. If the checker has ruled `none` out
    /// at this spot (it is inside `if x is not none { ... }`, say), give
    /// the value inside instead of the `T or none` around it.
    ///
    /// `changeable` is for a variable that other functions can reach and
    /// change. One of them may have set it back to `none` since the
    /// check, so it is looked at once more before it is trusted.
    fn present(&mut self, val: Val, expr: &Expr, changeable: bool) -> Result<Val, Error> {
        if !matches!(val.kind, Kind::Optional(_)) || matches!(self.types.narrowed(expr), None | Some(Kind::None)) {
            return Ok(val);
        }
        if changeable {
            let there = self.is_there(&val, expr.span)?;
            let n = self.number();
            let (here, gone) = (format!("here{n}"), format!("gone{n}"));
            self.jump_if(&there, &here, &gone);
            self.place(&gone);
            self.run("zeno_gone", at(expr.span));
            self.end("unreachable");
            self.place(&here);
        }
        self.inside(&val, expr.span)
    }

    /// `x is none`, `x is not none`
    fn is_none(&mut self, op: BinaryOp, left: &Expr) -> Result<Val, Error> {
        let val = self.expr(left)?;
        let is = op == BinaryOp::Is;
        let code = match &val.kind {
            Kind::Optional(_) => {
                let there = self.is_there(&val, left.span)?;
                if is {
                    self.set(format!("xor i1 {there}, true"))
                } else {
                    there
                }
            }
            // Anything else is either always `none` or never.
            other => ((*other == Kind::None) == is).to_string(),
        };
        Ok(Val::new(code, Kind::Bool))
    }

    // ---------------------------------------------------------------- Calls

    /// A call of a function of the program by its name.
    fn call_named(&mut self, entry: &Entry, args: &[Arg], span: Span) -> Result<Val, Error> {
        let slots = entry
            .decl
            .params
            .iter()
            .zip(&entry.kind.params)
            .map(|(param, kind)| (param.name.clone(), kind.clone(), param.default.clone()))
            .collect();
        self.callee = Some((entry.within, entry.sight.clone(), None));
        let list = self.bind(&slots, &entry.owned, &entry.lends, args, span)?;
        self.call_entry(entry, list, span)
    }

    fn call_expr(&mut self, callee: &Expr, args: &[Arg], span: Span) -> Result<Val, Error> {
        match &callee.kind {
            ExprKind::Name(name) if self.lookup(name).is_none() => {
                if let Some(entry) = self.functions.get(name).cloned() {
                    return self.call_named(&entry, args, span);
                }
                if self.classes.contains_key(name) {
                    return self.construct(name, args, span);
                }
                if let Some((owner, index)) = self.cases.get(name).cloned() {
                    return self.construct_case(&owner, index, args, span);
                }
                if self.imported.get(name).is_some_and(|module| module == "math") {
                    return self.math(name, args, span);
                }
                if let Some(module) = self.imported.get(name).cloned() {
                    return self.device(&module, name, args, span);
                }
                self.builtin(name, args, span)
            }
            ExprKind::Field { object, name, .. } => {
                if let ExprKind::Name(owner) = &object.kind {
                    if self.lookup(owner).is_none() {
                        // `math.sqrt(x)`
                        if owner == "math" {
                            return self.math(name, args, span);
                        }
                        // `time.sleep(1 s)`, `board.pin(7)`
                        if matches!(owner.as_str(), "time" | "board" | "files" | "program") {
                            return self.device(owner, name, args, span);
                        }
                        // `u8.wrap(n)`: cut `n` down to eight bits and
                        // make it an i64 again. Two instructions, no call.
                        if let (Some(width), "wrap", [arg]) = (self.sized_name(owner), name.as_str(), args) {
                            let n = self.expr(&arg.value)?;
                            if n.kind != Kind::Int || arg.name.is_some() {
                                return Err(cannot(format!("`{owner}.wrap` called this way"), span));
                            }
                            let small = self.set(format!("trunc i64 {} to {}", n.code, width.ir_type()));
                            let extend = if width.signed { "sext" } else { "zext" };
                            return Ok(Val::new(self.set(format!("{extend} {} {small} to i64", width.ir_type())), Kind::Int));
                        }
                        // `Shape.Circle(1.0)`
                        let case =
                            self.enums.get(owner).and_then(|entry| entry.cases.iter().position(|(case, _)| case == name));
                        if let Some(index) = case {
                            return self.construct_case(owner, index, args, span);
                        }
                    }
                }
                self.method(object, name, args, span)
            }
            // A function with a name written inside a function, held as a
            // closure (decision 56.16): called with names and defaults as
            // any function with a name is.
            ExprKind::Name(name) if self.lookup(name).is_some_and(|variable| variable.inner.is_some()) => {
                let variable = self.lookup(name).cloned().expect("looked up above");
                let (decl, usable) = variable.inner.clone().expect("looked up above");
                if !usable {
                    return Err(emit_c::inner_outlives(name, span));
                }
                let function = match &variable.place {
                    Place::Value(code) => Val::new(code.clone(), variable.kind.clone()),
                    Place::Slot(slot) => {
                        let ty = ir_type(&variable.kind, span)?;
                        Val::new(self.load(ty, slot), variable.kind.clone())
                    }
                };
                let args = emit_c::in_order(&decl, args, span, "`zeno build --llvm`")?;
                self.call_value(function.fixed(), &args, span)
            }
            // Anything else that is called must be a function value.
            _ => {
                let function = self.expr(callee)?;
                self.call_value(function, args, span)
            }
        }
    }

    /// The arguments of a call, each put in its place and written with
    /// its type, ready to go between the brackets. `slots` are the names,
    /// kinds and default values of what is being called.
    ///
    /// The arguments are worked out left to right as they are written,
    /// even when names put them elsewhere.
    ///
    /// `owned` says which places take over an owned value, and `lends`
    /// which only look (no `take`).
    fn bind(&mut self, slots: &Slots, owned: &[bool], lends: &[bool], args: &[Arg], span: Span) -> Result<Vec<String>, Error> {
        // Where the function was written, for its defaults: taken now, so
        // that a call among the arguments does not see it.
        let callee = self.callee.take();
        let mut given: Vec<Option<Val>> = vec![None; slots.len()];
        let mut held = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let place = match &arg.name {
                None => index,
                Some(name) => match slots.iter().position(|(slot, _, _)| slot == name) {
                    Some(place) => place,
                    None => return Err(cannot(format!("the argument `{name}`"), span)),
                },
            };
            let val = self.expr(&arg.value)?;
            // A parameter without `take` only looks, so it may be lent what
            // the statement owns. Anything else may keep what it is given.
            if !(val.passing && lends.get(place) == Some(&true)) {
                self.stays(&val, arg.value.span)?;
            }
            // An owned value on its way into the call has left its owner
            // and not yet reached the function. In a firmware build a
            // later argument may fail, and the failure leaves this
            // function at once. Hold the value until the call, so that
            // such a failure frees it.
            if self.firmware && owned.get(place) == Some(&true) && index + 1 < args.len() {
                held.push(self.hold(val.kind.clone(), &val.code));
            }
            match given.get_mut(place) {
                Some(slot) => *slot = Some(val),
                None => return Err(cannot("this many arguments", span)),
            }
        }
        // From here the function owns what it was given.
        for key in &held {
            self.let_go(key);
        }
        // Each argument is worked out already, so a default that is more
        // than a literal, worked out here in the order of the parameters,
        // can read the ones before it (decision 56.16).
        let mut fitted: Vec<Option<Val>> = Vec::new();
        for (val, (_, kind, _)) in given.into_iter().zip(slots) {
            fitted.push(match val {
                Some(val) => Some(self.fit(val, kind, span)?),
                None => None,
            });
        }
        let mut list = Vec::new();
        for place in 0..slots.len() {
            let (_, kind, default) = &slots[place];
            // An argument that was left out takes the default. A place
            // that keeps what it is given owns the list made for it
            // here; any other only looks, and the statement frees the
            // list after the call (as in `emit_c`).
            let keeps = !lends.get(place).copied().unwrap_or(false);
            let val = match (fitted[place].clone(), default) {
                (Some(val), _) => val,
                (None, Some(default)) if emit_c::is_literal(default) => {
                    let val = if keeps { self.value(default)? } else { self.expr(default)? };
                    self.fit(val, kind, span)?
                }
                (None, Some(default)) => {
                    let before: Vec<(String, Kind, String)> = slots[..place]
                        .iter()
                        .zip(&fitted)
                        .filter_map(|((name, kind, _), val)| val.as_ref().map(|val| (name.clone(), kind.clone(), val.code.clone())))
                        .collect();
                    let val = self.default_value(callee.as_ref(), default, keeps, &before)?;
                    let val = self.fit(val, kind, span)?;
                    fitted[place] = Some(val.clone());
                    val
                }
                (None, None) => return Err(cannot("this many arguments", span)),
            };
            list.push(format!("{} {}", ir_type(kind, span)?, val.code));
        }
        Ok(list)
    }

    /// A default that is not a literal, worked out at a call that leaves
    /// its argument out (decision 56.16, as in `emit_c`). It reads what it
    /// would read where the function is written: the file's variables and
    /// functions, and the parameters before it, which `before` holds, with
    /// `self` for a method. Nothing of the function that calls.
    fn default_value(
        &mut self,
        callee: Option<&(usize, Vec<(String, Entry)>, Option<Val>)>,
        default: &Expr,
        keeps: bool,
        before: &[(String, Kind, String)],
    ) -> Result<Val, Error> {
        let (within, sight, receiver) = callee.cloned().unwrap_or_default();
        let given = |code: &str, kind: &Kind| Variable {
            place: Place::Value(code.to_string()),
            kind: kind.clone(),
            open: false,
            owner: false,
            inner: None,
        };
        let mut params = HashMap::new();
        if let Some(object) = &receiver {
            params.insert("self".to_string(), given(&object.code, &object.kind));
        }
        for (name, kind, code) in before {
            params.insert(name.clone(), given(code, kind));
        }
        let mut outside = std::mem::take(&mut self.scopes);
        self.scopes = vec![outside.remove(0), params];
        let mut functions = std::mem::replace(&mut self.functions, self.top_functions.clone());
        for (name, entry) in sight {
            self.functions.insert(name, entry);
        }
        let within = std::mem::replace(&mut self.within, within);
        let once = self.once.take();
        let val = if keeps { self.value(default) } else { self.expr(default) };
        self.once = once;
        self.within = within;
        std::mem::swap(&mut self.functions, &mut functions);
        let mut inside = std::mem::take(&mut self.scopes);
        outside.insert(0, inside.remove(0));
        self.scopes = outside;
        val
    }

    /// Write the call to one of the program's functions or methods.
    fn call_entry(&mut self, entry: &Entry, mut list: Vec<String>, span: Span) -> Result<Val, Error> {
        list.extend(self.outs_for(&entry.kind.returns, span)?);
        if self.firmware && !entry.decl.raises.is_empty() {
            // It says whether it worked, and that is tested at once.
            let call = format!("call zeroext i1 {}({}", entry.name, list.join(", "));
            return self.checked_call(call, &entry.kind.returns, &entry.decl.raises, span);
        }
        let returns = return_type(&entry.kind.returns, span)?;
        let call = format!("call {returns} {}({})", entry.name, list.join(", "));
        Ok(match &entry.kind.returns {
            // Nothing comes back, so there is nothing to name.
            Kind::None => {
                self.ins(call);
                Val::nothing()
            }
            kind => {
                let val = Val::new(self.set(call), kind.clone());
                self.came_back(val)
            }
        })
    }

    /// For the call of a function that gives several values, where the
    /// later ones go: what `Receive` or `return` said (decision 57).
    fn outs_for(&mut self, returns: &Kind, span: Span) -> Result<Vec<String>, Error> {
        if emit_c::later_values(returns).is_empty() {
            return Ok(Vec::new());
        }
        self.outs.take().ok_or_else(|| cannot("several values given anywhere but to as many places", span))
    }

    /// What a function gave back. With a collector, text that comes back
    /// will last. With none it is in the text room, like text this
    /// statement built itself.
    fn came_back(&self, val: Val) -> Val {
        if !self.collector && is_text(emit_c::first_value(&val.kind)) { val } else { val.fixed() }
    }

    /// `sqrt`, `floor` and the rest of `import math`.
    /// A function of the `time`, the `board` or the `files` module. Each
    /// is one call into the runtime, which passes it on to the board, or
    /// to the C library where the files are.
    fn device(&mut self, module: &str, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        // `register(u32, 0x4002_0014)`: a register is its address.
        if (module, name) == ("board", "register") {
            let (pointee, address) = written_type("register", args, span)?;
            let address = self.expr(address)?;
            return match (pointee, &address.kind) {
                (Pointee::Sized(width), Kind::Int) => Ok(Val { kind: Kind::Register(width), ..address }),
                _ => Err(cannot("`register` called this way", span)),
            };
        }
        let mut vals = Vec::new();
        for arg in args {
            if arg.name.is_some() {
                return Err(cannot(format!("`{module}.{name}` called this way"), span));
            }
            vals.push(self.expr(&arg.value)?);
        }
        match (module, name, vals.as_slice()) {
            // The board counts in seconds, whatever unit the program wrote.
            ("time", "sleep", [time]) => {
                let scale = match &time.kind {
                    Kind::Quantity(unit) => units::convert(unit, "s"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`sleep` of something that is not a time", span));
                };
                let seconds = self.scaled(&time.code, scale);
                self.run("zeno_sleep", format!("double {seconds}"));
                Ok(Val::nothing())
            }
            ("time", "now", []) => Ok(Val::new(self.call("double", "zeno_now", ""), Kind::Quantity("ms".to_string()))),
            ("board", "pin", [number]) if number.kind == Kind::Int => {
                Ok(Val::new(self.call("i64", "zeno_pin", format!("i64 {}, {}", number.code, at(span))), Kind::Pin))
            }
            // `ticker(10 ms)`: the period goes over in milliseconds.
            ("board", "ticker", [time]) => {
                let scale = match &time.kind {
                    Kind::Quantity(unit) => units::convert(unit, "ms"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`ticker` with something that is not a time", span));
                };
                let period = self.scaled(&time.code, scale);
                Ok(Val::new(self.call("i64", "zeno_ticker", format!("double {period}, {}", at(span))), Kind::Ticker))
            }
            // `pwm_pin(9, 1 kHz)`: the frequency goes over in hertz.
            ("board", "pwm_pin", [number, rate]) if number.kind == Kind::Int => {
                let scale = match &rate.kind {
                    Kind::Quantity(unit) => units::convert(unit, "Hz"),
                    _ => None,
                };
                let Some(scale) = scale else {
                    return Err(cannot("`pwm_pin` with something that is not a frequency", span));
                };
                let hertz = self.scaled(&rate.code, scale);
                let made = format!("i64 {}, double {hertz}, {}", number.code, at(span));
                Ok(Val::new(self.call("i64", "zeno_pwm_pin", made), Kind::PwmPin))
            }
            // `analog_pin(0)`.
            ("board", "analog_pin", [number]) if number.kind == Kind::Int => {
                let made = format!("i64 {}, {}", number.code, at(span));
                Ok(Val::new(self.call("i64", "zeno_analog_pin", made), Kind::AnalogPin))
            }
            // `serial(1, 115200 baud)`.
            ("board", "serial", [number, speed]) if number.kind == Kind::Int && speed.kind == Kind::Quantity("baud".to_string()) => {
                let made = format!("i64 {}, double {}, {}", number.code, speed.code, at(span));
                Ok(Val::new(self.call("i64", "zeno_serial", made), Kind::Serial))
            }
            // `input_pin(2, pull_up)`: the pull is the str of its name,
            // or null when nothing pulls the pin.
            ("board", "input_pin", [number, rest @ ..]) if number.kind == Kind::Int && rest.len() < 2 => {
                let pull = rest.first().map_or("null".to_string(), |pull| pull.code.clone());
                let made = format!("i64 {}, ptr {pull}, {}", number.code, at(span));
                Ok(Val::new(self.call("i64", "zeno_input_pin", made), Kind::InputPin))
            }
            // `files.exists(path)` cannot fail: it is a question.
            ("files", "exists", [path]) if path.kind == Kind::Str => {
                Ok(Val::new(self.call("i1", "zeno_files_exists", format!("ptr {}", path.code)), Kind::Bool))
            }
            // The other four can: the file may not be there, or may not
            // be text. Each is one call into the runtime, which raises a
            // `FileError`. In a firmware build the call says whether it
            // worked, as `int(text)` does.
            ("files", "read" | "write" | "append" | "remove", texts)
                if texts.iter().all(|text| text.kind == Kind::Str)
                    && texts.len() == if matches!(name, "write" | "append") { 2 } else { 1 } =>
            {
                let runtime = match name {
                    "read" => "zeno_files_read",
                    "write" => "zeno_files_write",
                    "append" => "zeno_files_append",
                    _ => "zeno_files_remove",
                };
                let returns = if name == "read" { Kind::Str } else { Kind::None };
                let mut list = String::new();
                for text in texts {
                    list += &format!("ptr {}, ", text.code);
                }
                list += &at(span);
                // The runtime builds text on the way: the file's own, or
                // the message of the error.
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses(runtime);
                    let call = format!("call zeroext i1 @{runtime}({list}");
                    return self.checked_call(call, &returns, &["FileError".to_string()], span);
                }
                if name == "read" {
                    let text = Val::new(self.call("ptr", runtime, list), Kind::Str);
                    return Ok(self.came_back(text));
                }
                self.run(runtime, list);
                Ok(Val::nothing())
            }
            // `files.read_bytes(path)`: a new list[u8], which is nobody's
            // yet. `files.write_bytes(path, bytes)` lends the list to the
            // runtime for the call (decision 36.1).
            ("files", "read_bytes", [path]) if path.kind == Kind::Str => {
                let returns = Kind::List(Box::new(Kind::Byte));
                let list = format!("ptr {}, {}", path.code, at(span));
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses("zeno_files_read_bytes");
                    let call = format!("call zeroext i1 @zeno_files_read_bytes({list}");
                    let bytes = self.checked_call(call, &returns, &["FileError".to_string()], span)?;
                    return Ok(bytes.made());
                }
                let bytes = Val::new(self.call("ptr", "zeno_files_read_bytes", list), returns);
                Ok(self.came_back(bytes).made())
            }
            ("files", "write_bytes", [path, bytes])
                if path.kind == Kind::Str && bytes.kind == Kind::List(Box::new(Kind::Byte)) =>
            {
                let list = format!("ptr {}, ptr {}, {}", path.code, bytes.code, at(span));
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses("zeno_files_write_bytes");
                    let call = format!("call zeroext i1 @zeno_files_write_bytes({list}");
                    return self.checked_call(call, &Kind::None, &["FileError".to_string()], span);
                }
                self.run("zeno_files_write_bytes", list);
                Ok(Val::nothing())
            }
            // `files.list(folder)`: a new list of text, which is nobody's
            // yet. `files.make_folder(path)`. The part of the runtime
            // they are in is written when the IR calls one (59.20, 59.21).
            ("files", "list", [folder]) if folder.kind == Kind::Str => {
                let returns = Kind::List(Box::new(Kind::Str));
                let list = format!("ptr {}, {}", folder.code, at(span));
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses("zeno_files_list");
                    let call = format!("call zeroext i1 @zeno_files_list({list}");
                    let names = self.checked_call(call, &returns, &["FileError".to_string()], span)?;
                    return Ok(names.made());
                }
                let names = Val::new(self.call("ptr", "zeno_files_list", list), returns);
                Ok(self.came_back(names).made())
            }
            ("files", "make_folder", [path]) if path.kind == Kind::Str => {
                let list = format!("ptr {}, {}", path.code, at(span));
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses("zeno_files_make_folder");
                    let call = format!("call zeroext i1 @zeno_files_make_folder({list}");
                    return self.checked_call(call, &Kind::None, &["FileError".to_string()], span);
                }
                self.run("zeno_files_make_folder", list);
                Ok(Val::nothing())
            }
            // What a `files.Reader` and a `files.Writer` are made of
            // (decision 59.6). The number of an open file is the address
            // of its `FILE`, an i64. The part of the runtime they are in
            // is written when the IR calls one.
            ("files", "handle_open" | "handle_line" | "handle_bytes" | "handle_write" | "handle_write_bytes", _)
                if emit_c::handle_arguments(name, &vals.iter().map(|val| &val.kind).collect::<Vec<_>>()) =>
            {
                let runtime = match name {
                    "handle_open" => "zeno_files_handle_open",
                    "handle_line" => "zeno_files_handle_line",
                    "handle_bytes" => "zeno_files_handle_bytes",
                    "handle_write" => "zeno_files_handle_write",
                    _ => "zeno_files_handle_write_bytes",
                };
                let mut list = String::new();
                for val in &vals {
                    let ty = if val.kind == Kind::Int { "i64" } else { "ptr" };
                    list += &format!("{ty} {}, ", val.code);
                }
                list += &at(span);
                let returns = match name {
                    "handle_open" => Kind::Int,
                    "handle_line" => Kind::Optional(Box::new(Kind::Str)),
                    "handle_bytes" => Kind::List(Box::new(Kind::Byte)),
                    _ => Kind::None,
                };
                // The message of an error, and a line, are built in the
                // text room.
                self.code.built = !self.collector;
                if self.firmware {
                    self.uses(runtime);
                    let call = format!("call zeroext i1 @{runtime}({list}");
                    let val = self.checked_call(call, &returns, &["FileError".to_string()], span)?;
                    return Ok(if name == "handle_bytes" { val.made() } else { val });
                }
                Ok(match name {
                    "handle_open" => Val::new(self.call("i64", runtime, list), Kind::Int),
                    "handle_line" | "handle_bytes" => {
                        let val = Val::new(self.call("ptr", runtime, list), returns);
                        let val = self.came_back(val);
                        if name == "handle_bytes" { val.made() } else { val }
                    }
                    _ => {
                        self.run(runtime, list);
                        Val::nothing()
                    }
                })
            }
            ("files", "handle_close", [file]) if file.kind == Kind::Int => {
                self.run("zeno_files_handle_close", format!("i64 {}", file.code));
                Ok(Val::nothing())
            }
            // `program.arguments()`: a new list, which is nobody's yet.
            // `main` hands the runtime the command line: see `file`.
            ("program", "arguments", []) => {
                self.uses("zeno_program_start");
                let words = self.call("ptr", "zeno_program_arguments", "");
                Ok(Val::new(words, Kind::List(Box::new(Kind::Str))).made())
            }
            // `program.exit(code)`: C's own `exit`, after the code is
            // checked (decision 34.1).
            ("program", "exit", [code]) if code.kind == Kind::Int => {
                self.run("zeno_program_exit", format!("i64 {}, {}", code.code, at(span)));
                Ok(Val::nothing())
            }
            // `program.environment(name)`: a copy of the text, or null.
            ("program", "environment", [name]) if name.kind == Kind::Str => {
                self.code.built = !self.collector;
                let value = self.call("ptr", "zeno_program_environment", format!("ptr {}", name.code));
                let value = Val::new(value, Kind::Optional(Box::new(Kind::Str)));
                Ok(self.came_back(value))
            }
            ("program", "warn", [text]) if text.kind == Kind::Str => {
                self.run("zeno_program_warn", format!("ptr {}", text.code));
                Ok(Val::nothing())
            }
            // `program.run(name, words)` starts another program, which
            // only `zeno run` does so far.
            ("program", "run", _) => Err(cannot("`program.run`", span)),
            _ => Err(cannot(format!("`{module}.{name}` called this way"), span)),
        }
    }

    fn math(&mut self, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let wrong = || cannot(format!("`{name}` called this way"), span);
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(wrong());
        }
        // An angle that a function works out is in radians, and says so.
        let radians = Kind::Quantity("rad".to_string());
        // `atan2(y, x)` is the one function of two numbers.
        if let ("atan2", [y, x]) = (name, args) {
            let (y, x) = (self.expr(&y.value)?, self.expr(&x.value)?);
            if !matches!(y.kind, Kind::Int | Kind::Float) || !matches!(x.kind, Kind::Int | Kind::Float) {
                return Err(wrong());
            }
            let (y, x) = (self.double(&y), self.double(&x));
            return Ok(Val::new(self.call("double", "atan2", format!("double {y}, double {x}")), radians));
        }
        let [arg] = args else {
            return Err(wrong());
        };
        let val = self.expr(&arg.value)?;
        let x = match &val.kind {
            // `sin(30 deg)`: the C library counts in radians.
            Kind::Quantity(unit) if matches!(name, "sin" | "cos" | "tan") => match units::convert(unit, "rad") {
                Some(scale) => self.scaled(&val.code, scale),
                None => return Err(cannot(format!("`{name}` of a quantity in {unit}"), span)),
            },
            Kind::Int | Kind::Float => self.double(&val),
            _ => return Err(wrong()),
        };
        Ok(match name {
            "sqrt" => Val::new(self.call("double", "zeno.sqrt", format!("double {x}, {}", at(span))), Kind::Float),
            // The C library rounds, and the runtime turns the float that
            // comes back into an int.
            "floor" | "ceil" => {
                let rounded = self.call("double", if name == "floor" { "floor" } else { "ceil" }, format!("double {x}"));
                Val::new(self.call("i64", "zeno_whole", format!("double {rounded}")), Kind::Int)
            }
            // C's `round` sends a half away from zero, as Zeno's does.
            "round" => {
                let rounded = self.call("double", "round", format!("double {x}"));
                Val::new(self.call("i64", "zeno_whole", format!("double {rounded}")), Kind::Int)
            }
            "sin" => Val::new(self.call("double", "sin", format!("double {x}")), Kind::Float),
            "cos" => Val::new(self.call("double", "cos", format!("double {x}")), Kind::Float),
            "tan" => Val::new(self.call("double", "tan", format!("double {x}")), Kind::Float),
            "exp" => Val::new(self.call("double", "exp", format!("double {x}")), Kind::Float),
            "atan" => Val::new(self.call("double", "atan", format!("double {x}")), radians),
            // These two refuse a number that is the sine of no angle.
            "asin" => Val::new(self.call("double", "zeno_asin", format!("double {x}, {}", at(span))), radians),
            "acos" => Val::new(self.call("double", "zeno_acos", format!("double {x}, {}", at(span))), radians),
            // And these a number that has no logarithm.
            "log" | "log2" | "log10" => {
                let base = match name {
                    "log2" => 2,
                    "log10" => 10,
                    _ => 0,
                };
                let args = format!("double {x}, i32 signext {base}, {}", at(span));
                Val::new(self.call("double", "zeno_log", args), Kind::Float)
            }
            _ => return Err(cannot(format!("`math.{name}`"), span)),
        })
    }

    /// A call to one of the functions Zeno comes with.
    fn builtin(&mut self, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(cannot(format!("`{name}` with a named argument"), span));
        }
        // `allocate(u8, 16)`: sixteen bytes, all zero. The runtime takes
        // them from the heap, or from the pool where there is no
        // collector. It is told how many items, and the bytes in one.
        if name == "allocate" {
            let (pointee, count) = written_type("allocate", args, span)?;
            let count = self.expr(count)?;
            if count.kind != Kind::Int {
                return Err(cannot("`allocate` of a number of items that is not an int", span));
            }
            let args = format!("i64 {}, i64 {}, {}", count.code, pointee.bytes(), at(span));
            return Ok(Val::new(self.call("ptr", "zeno_allocate", args), Kind::Pointer(pointee)));
        }
        let mut vals = Vec::new();
        for arg in args {
            vals.push(self.expr(&arg.value)?);
        }
        let wrong = || cannot(format!("`{name}` used this way"), span);
        Ok(match (name, vals.as_slice()) {
            // `free(p)`: back to the heap, or to the pool.
            ("free", [pointer]) if matches!(pointer.kind, Kind::Pointer(_)) => {
                self.run("zeno_release", format!("ptr {}", pointer.code));
                Val::nothing()
            }
            ("print", _) => {
                // `zeno_print` takes a count and then that many pieces of
                // text. `(i32, ...)` says the call has a varying number
                // of arguments, which IR must be told at each call.
                let mut list = format!("i32 {}", vals.len());
                for val in &vals {
                    list += &format!(", ptr {}", self.text(val, span)?);
                }
                self.uses("zeno_print");
                self.ins(format!("call void (i32, ...) @zeno_print({list})"));
                Val::nothing()
            }
            // `u8(n)`: `n` itself, once the runtime has seen that it fits.
            // The size goes along in words, for the message.
            (sized, [n]) if ints::width(sized).is_some() && n.kind == Kind::Int => {
                let width = ints::width(sized).unwrap_or(ints::WIDTHS[0]);
                let said = self.string(&format!("{} {sized}", if width.signed { "an" } else { "a" }));
                let args = format!("i64 {}, i64 {}, i64 {}, ptr {said}, {}", n.code, width.min(), width.max(), at(span));
                Val::new(self.call("i64", "zeno_fit", args), Kind::Int)
            }
            // `ValueError("bad")` makes an error value: its kind, its
            // message, and no object.
            (kind, _) if emit_c::ERROR_KINDS.contains(&kind) => {
                let message = match vals.as_slice() {
                    [] => self.string(""),
                    [message] => self.text(message, span)?,
                    _ => return Err(cannot(format!("`{name}` with more than one message"), span)),
                };
                let kind = self.string(kind);
                let error = self.call("ptr", "zeno_error_new", format!("ptr {kind}, ptr {message}, ptr null"));
                Val::new(error, Kind::Error)
            }
            ("str", [val]) => Val::new(self.text(val, span)?, Kind::Str),
            ("len", [val]) if val.kind == Kind::Str => {
                Val::new(self.call("i64", "zeno_len", format!("ptr {}", val.code)), Kind::Int)
            }
            // A list starts with how many items it has.
            ("len", [val]) if matches!(val.kind, Kind::List(_)) => {
                Val::new(self.set(format!("load i64, ptr {}", val.code)), Kind::Int)
            }
            // A dict starts with its list of keys, and has as many
            // entries as that list has items.
            ("len", [val]) if matches!(val.kind, Kind::Dict(..)) => {
                let keys = self.set(format!("load ptr, ptr {}", val.code));
                Val::new(self.set(format!("load i64, ptr {keys}")), Kind::Int)
            }
            // `char(65)`: the runtime writes the bytes of that character.
            // `input()`, `input("Name: ")`: the runtime reads a line, and
            // gives null, which is `none`, when there are no more.
            ("input", []) | ("input", [Val { kind: Kind::Str, .. }]) => {
                let question = vals.first().map_or("null", |val| val.code.as_str());
                self.code.built = !self.collector;
                let line = Val::new(self.call("ptr", "zeno_input", format!("ptr {question}")), Kind::Optional(Box::new(Kind::Str)));
                self.came_back(line)
            }
            // `range(5)` used as a value: the list of numbers that `zeno
            // run` makes. A `for` over a range builds none.
            ("range", [_] | [_, _] | [_, _, _]) if vals.iter().all(|val| val.kind == Kind::Int) => {
                let (start, stop, step) = match vals.as_slice() {
                    [stop] => ("0", stop.code.as_str(), "1"),
                    [start, stop] => (start.code.as_str(), stop.code.as_str(), "1"),
                    [start, stop, step, ..] => (start.code.as_str(), stop.code.as_str(), step.code.as_str()),
                    [] => return Err(wrong()),
                };
                let args = format!("i64 {start}, i64 {stop}, i64 {step}, {}", at(span));
                Val::new(self.call("ptr", "zeno_range_list", args), Kind::List(Box::new(Kind::Int))).made()
            }
            ("char", [val]) if val.kind == Kind::Int => {
                Val::new(self.call("ptr", "zeno_char", format!("i64 {}, {}", val.code, at(span))), Kind::Str)
            }
            // `u64(n)`: an int below 0 is an OverflowError (decision 42.4).
            // `u64..of` is the checker's name for `u64.wrap`, and for a
            // u64 written out: the same 64 bits.
            ("u64" | "u64..of", [val]) => match val.kind {
                Kind::U64 => val.clone(),
                Kind::Int if name == "u64..of" || val.code.parse::<u64>().is_ok() => Val::new(val.code.clone(), Kind::U64),
                Kind::Int => Val::new(self.call("i64", "zeno_u64", format!("i64 {}, {}", val.code, at(span))), Kind::U64),
                _ => return Err(wrong()),
            },
            // `u64.wrap(a + b)`: the plain instruction, which wraps.
            ("u64..add" | "u64..sub" | "u64..mul", [a, b]) => {
                let plain = match name { "u64..add" => "add", "u64..sub" => "sub", _ => "mul" };
                Val::new(self.set(format!("{plain} i64 {}, {}", a.code, b.code)), Kind::U64)
            }
            ("u64..shl", [a, b]) => {
                let args = format!("i64 {}, i64 {}, i1 {}, {}", a.code, b.code, b.kind == Kind::Int, at(span));
                Val::new(self.call("i64", "zeno_shl_wrap_u64", args), Kind::U64)
            }
            ("abs", [val]) if val.kind == Kind::U64 => val.clone(),
            ("abs", [val]) => match val.kind {
                Kind::Int => Val::new(self.call("i64", "zeno_abs", format!("i64 {}, {}", val.code, at(span))), Kind::Int),
                Kind::Float | Kind::F32 | Kind::Quantity(_) => {
                    Val::new(self.call("double", "fabs", format!("double {}", val.code)), val.kind.clone())
                }
                _ => return Err(wrong()),
            },
            ("int", [val]) => match val.kind {
                Kind::Int => val.clone(),
                Kind::U64 => Val::new(self.call("i64", "zeno_int_u64", format!("i64 {}, {}", val.code, at(span))), Kind::Int),
                Kind::Float | Kind::F32 => {
                    Val::new(self.call("i64", "zeno_int", format!("double {}, {}", val.code, at(span))), Kind::Int)
                }
                // Text may not be a number at all, so this one can raise.
                Kind::Str if self.firmware => {
                    // The runtime builds text of its own on the way.
                    self.code.built = !self.collector;
                    self.uses("zeno_parse_int");
                    let call = format!("call zeroext i1 @zeno_parse_int(ptr {}, {}", val.code, at(span));
                    self.checked_call(call, &Kind::Int, &["ValueError".to_string()], span)?
                }
                Kind::Str => {
                    Val::new(self.call("i64", "zeno_parse_int", format!("ptr {}, {}", val.code, at(span))), Kind::Int)
                }
                _ => return Err(wrong()),
            },
            ("float", [val]) => match val.kind {
                Kind::Int | Kind::Float | Kind::U64 => Val::new(self.double(val), Kind::Float),
                // An f32 is held as the double it is already.
                Kind::F32 => Val::new(val.code.clone(), Kind::Float),
                Kind::Str if self.firmware => {
                    self.code.built = !self.collector;
                    self.uses("zeno_parse_float");
                    let call = format!("call zeroext i1 @zeno_parse_float(ptr {}, {}", val.code, at(span));
                    self.checked_call(call, &Kind::Float, &["ValueError".to_string()], span)?
                }
                Kind::Str => Val::new(
                    self.call("double", "zeno_parse_float", format!("ptr {}, {}", val.code, at(span))),
                    Kind::Float,
                ),
                _ => return Err(wrong()),
            },
            // `f32(x)`: the nearest f32 (decision 41.5). A float written
            // out was checked to fit when the program was.
            ("f32", [val]) => match val.kind {
                Kind::F32 => val.clone(),
                Kind::Int => Val::new(self.single(val), Kind::F32),
                Kind::Float if val.code.parse::<f64>().is_ok() || val.code.starts_with("0x") => {
                    Val::new(self.rounded(&val.code), Kind::F32)
                }
                Kind::Float => {
                    let near = self.call("float", "zeno_f32", format!("double {}, {}", val.code, at(span)));
                    Val::new(self.set(format!("fpext float {near} to double")), Kind::F32)
                }
                _ => return Err(wrong()),
            },
            ("sum", [Val { kind: Kind::List(item), code, .. }]) if **item == Kind::F32 => {
                let total = self.call("float", "zeno_list_sum_f32", format!("ptr {code}"));
                Val::new(self.set(format!("fpext float {total} to double")), Kind::F32)
            }
            ("min" | "max", [Val { kind: Kind::List(item), code, .. }]) if **item == Kind::F32 => {
                let said = self.string(name);
                let args = format!("ptr {code}, i1 {}, ptr {said}, {}", name == "min", at(span));
                Val::new(self.call("double", "zeno_list_best_float", args), Kind::F32)
            }
            // `sum`, `min` and `max` of one list of numbers.
            ("sum" | "min" | "max", [Val { kind: Kind::List(item), code, .. }]) => {
                let (returns, function, args) = match (name, &**item) {
                    ("sum", Kind::Byte) => ("i64", "zeno_bytes_sum", format!("ptr {code}")),
                    (_, Kind::Byte) => {
                        let said = self.string(name);
                        ("i64", "zeno_bytes_best", format!("ptr {code}, i1 {}, ptr {said}, {}", name == "min", at(span)))
                    }
                    ("sum", Kind::Int) => ("i64", "zeno_list_sum_int", format!("ptr {code}, {}", at(span))),
                    ("sum", Kind::U64) => ("i64", "zeno_list_sum_u64", format!("ptr {code}, {}", at(span))),
                    ("sum", Kind::Float) => ("double", "zeno_list_sum_float", format!("ptr {code}")),
                    (_, Kind::Int | Kind::Float | Kind::U64) | ("min" | "max", Kind::Quantity(_)) => {
                        // The runtime is told which of the two is wanted,
                        // and the name, for its message about an empty list.
                        let said = self.string(name);
                        let args = format!("ptr {code}, i1 {}, ptr {said}, {}", name == "min", at(span));
                        match **item {
                            Kind::Int => ("i64", "zeno_list_best_int", args),
                            Kind::U64 => ("i64", "zeno_list_best_u64", args),
                            _ => ("double", "zeno_list_best_float", args),
                        }
                    }
                    _ => return Err(cannot(format!("`{name}` of a list of anything but ints or floats"), span)),
                };
                Val::new(self.call(returns, function, args), read_kind(item))
            }
            // `min(gain, 1)`: f32s, perhaps with ints, give an f32.
            ("min" | "max", [first, rest @ ..])
                if !rest.is_empty()
                    && vals.iter().any(|val| val.kind == Kind::F32)
                    && vals.iter().all(|val| matches!(val.kind, Kind::F32 | Kind::Int)) =>
            {
                let compare = if name == "min" { "fcmp olt" } else { "fcmp ogt" };
                let mut best = self.single(first);
                for val in rest {
                    let next = self.single(val);
                    let better = self.set(format!("{compare} double {next}, {best}"));
                    best = self.set(format!("select i1 {better}, double {next}, double {best}"));
                }
                Val::new(best, Kind::F32)
            }
            ("min" | "max", [first, rest @ ..])
                if !rest.is_empty()
                    && matches!(first.kind, Kind::Int | Kind::U64 | Kind::Float | Kind::Quantity(_))
                    && rest.iter().all(|val| val.kind == first.kind) =>
            {
                // `select` picks one of two values by a bool, with no
                // jump: `b < a ? b : a`. The earlier value is kept when
                // the two are equal.
                let (compare, ty) = match (&first.kind, name) {
                    (Kind::Int, "min") => ("icmp slt", "i64"),
                    (Kind::Int, _) => ("icmp sgt", "i64"),
                    (Kind::U64, "min") => ("icmp ult", "i64"),
                    (Kind::U64, _) => ("icmp ugt", "i64"),
                    (_, "min") => ("fcmp olt", "double"),
                    _ => ("fcmp ogt", "double"),
                };
                let mut best = first.code.clone();
                for val in rest {
                    let better = self.set(format!("{compare} {ty} {}, {best}", val.code));
                    best = self.set(format!("select i1 {better}, {ty} {}, {ty} {best}", val.code));
                }
                Val::new(best, first.kind.clone())
            }
            _ => return Err(wrong()),
        })
    }

    /// The size called `name`, if `name` is still the built-in one and not
    /// something of the program's own.
    fn sized_name(&self, name: &str) -> Option<Width> {
        let taken = self.functions.contains_key(name) || self.classes.contains_key(name) || self.enums.contains_key(name);
        ints::width(name).filter(|_| !taken)
    }

    /// `object.name(args)`: a method of a class, of a str, or of a list.
    fn method(&mut self, object: &Expr, name: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let makes_bytes = std::mem::take(&mut self.makes_bytes);
        let receiver = self.expr(object)?;
        match receiver.kind.clone() {
            Kind::Class(class) => {
                let Some(entry) = self.classes[&class].methods.get(name).cloned() else {
                    // Not a method. It may be a field that holds a function.
                    let Some((ty, index, kind @ Kind::Function(_))) = self.field_of(&class, name) else {
                        return Err(cannot(format!("the method `{name}`"), span));
                    };
                    let at = self.field_at(&ty, &receiver.code, self.classes[&class].at(index));
                    let function = Val::new(self.set(format!("load ptr, ptr {at}")), kind);
                    return self.call_value(function, args, span);
                };
                // The first parameter is `self`, which the receiver fills.
                let slots = entry
                    .decl
                    .params
                    .iter()
                    .zip(&entry.kind.params)
                    .skip(1)
                    .map(|(param, kind)| (param.name.clone(), kind.clone(), param.default.clone()))
                    .collect();
                let mut list = vec![format!("ptr {}", receiver.code)];
                self.callee = Some((entry.within, Vec::new(), Some(receiver.clone())));
                list.extend(self.bind(&slots, &entry.owned[1..], &entry.lends[1..], args, span)?);
                self.call_entry(&entry, list, span)
            }
            // A register of the board: one call into the runtime, with
            // the address and how many bits wide the register is.
            Kind::Register(width) => {
                let register = format!("i64 {}, i32 signext {}", receiver.code, width.bits);
                match (name, args) {
                    ("read", []) => Ok(Val::new(self.call("i64", "zeno_register_read", register), Kind::Int)),
                    ("write", [Arg { name: None, value }]) => {
                        let value = self.expr(value)?;
                        if value.kind != Kind::Int {
                            return Err(cannot("`write` on a register with anything but a whole number", span));
                        }
                        self.run("zeno_register_write", format!("{register}, i64 {}", value.code));
                        Ok(Val::nothing())
                    }
                    _ => Err(cannot(format!("`{name}` on a register called this way"), span)),
                }
            }
            // A pin of the board: `led.on()`. Each is one call into the
            // runtime, with the pin's number.
            Kind::Pin => {
                if !args.is_empty() {
                    return Err(cannot(format!("`{name}` on a pin with arguments"), span));
                }
                let pin = format!("i64 {}", receiver.code);
                match name {
                    "on" => self.run("zeno_pin_on", pin),
                    "off" => self.run("zeno_pin_off", pin),
                    "toggle" => self.run("zeno_pin_toggle", pin),
                    "is_on" => return Ok(Val::new(self.call("i1", "zeno_pin_is_on", pin), Kind::Bool)),
                    _ => return Err(cannot(format!("the method `{name}`"), span)),
                }
                Ok(Val::nothing())
            }
            // `beat.wait()`, `beat.ticks()` and `beat.restart()`.
            Kind::Ticker => {
                let ticker = format!("i64 {}", receiver.code);
                match (name, args) {
                    ("wait", []) => Ok(Val::new(self.call("i64", "zeno_ticker_wait", ticker), Kind::Int)),
                    ("ticks", []) => Ok(Val::new(self.call("i64", "zeno_ticker_ticks", ticker), Kind::Int)),
                    ("restart", []) => {
                        self.run("zeno_ticker_restart", ticker);
                        Ok(Val::nothing())
                    }
                    _ => Err(cannot(format!("the method `{name}` of a ticker"), span)),
                }
            }
            // `lamp.duty(0.25)` and `servo.pulse(1500 us)`.
            Kind::PwmPin => match (name, args) {
                ("duty", [Arg { name: None, value }]) => {
                    let share = self.expr(value)?;
                    if share.kind != Kind::Float {
                        return Err(cannot("`duty` with anything but a float", span));
                    }
                    self.run("zeno_pwm_duty", format!("i64 {}, double {}, {}", receiver.code, share.code, at(span)));
                    Ok(Val::nothing())
                }
                ("pulse", [Arg { name: None, value }]) => {
                    let time = self.expr(value)?;
                    let scale = match &time.kind {
                        Kind::Quantity(unit) => units::convert(unit, "s"),
                        _ => None,
                    };
                    let Some(scale) = scale else {
                        return Err(cannot("`pulse` of something that is not a time", span));
                    };
                    let seconds = self.scaled(&time.code, scale);
                    self.run("zeno_pwm_pulse", format!("i64 {}, double {seconds}, {}", receiver.code, at(span)));
                    Ok(Val::nothing())
                }
                _ => Err(cannot(format!("`{name}` on a PWM pin called this way"), span)),
            },
            // `sensor.voltage()` and `sensor.count()`.
            Kind::AnalogPin => {
                let pin = format!("i64 {}", receiver.code);
                match (name, args) {
                    ("voltage", []) => Ok(Val::new(self.call("double", "zeno_analog_voltage", pin), Kind::Quantity("V".to_string()))),
                    ("count", []) => Ok(Val::new(self.call("i64", "zeno_analog_count", pin), Kind::Int)),
                    _ => Err(cannot(format!("the method `{name}` of an analog pin"), span)),
                }
            }
            // A serial port: each method is one call into the runtime.
            // `read_byte` gets its flag here, from the -1 of no byte.
            Kind::Serial => {
                let port = format!("i64 {}", receiver.code);
                match (name, args) {
                    ("write" | "write_bytes", [Arg { name: None, value }]) => {
                        let value = self.expr(value)?;
                        let fits = if name == "write" { value.kind == Kind::Str } else { value.kind == Kind::List(Box::new(Kind::Byte)) };
                        if !fits {
                            return Err(cannot(format!("`{name}` on a serial port called this way"), span));
                        }
                        let send = if name == "write" { "zeno_serial_write" } else { "zeno_serial_write_bytes" };
                        self.run(send, format!("{port}, ptr {}", value.code));
                        Ok(Val::nothing())
                    }
                    ("has_byte", []) => Ok(Val::new(self.call("i1", "zeno_serial_has_byte", port), Kind::Bool)),
                    ("read_byte", []) => {
                        let byte = self.call("i64", "zeno_serial_read_byte", port);
                        let present = self.set(format!("icmp sge i64 {byte}, 0"));
                        let flagged = self.set(format!("insertvalue {{ i1, i64 }} undef, i1 {present}, 0"));
                        let whole = self.set(format!("insertvalue {{ i1, i64 }} {flagged}, i64 {byte}, 1"));
                        Ok(Val::new(whole, Kind::Optional(Box::new(Kind::Int))))
                    }
                    _ => Err(cannot(format!("`{name}` on a serial port called this way"), span)),
                }
            }
            // `button.interrupt(falling)`: the edge is the str of its name.
            Kind::InputPin if name == "interrupt" => match args {
                [Arg { name: None, value }] => {
                    let edge = self.expr(value)?;
                    if edge.kind != Kind::Str {
                        return Err(cannot("`interrupt` with anything but an edge", span));
                    }
                    let made = format!("i64 {}, ptr {}, {}", receiver.code, edge.code, at(span));
                    Ok(Val::new(self.call("i64", "zeno_interrupt", made), Kind::Interrupt))
                }
                _ => Err(cannot("`interrupt` called this way", span)),
            },
            // `press.wait()` and `press.edges()`.
            Kind::Interrupt => match (name, args) {
                ("edges", []) => Ok(Val::new(self.call("i64", "zeno_interrupt_edges", format!("i64 {}", receiver.code)), Kind::Int)),
                ("wait", []) => {
                    let waited = format!("i64 {}, {}", receiver.code, at(span));
                    Ok(Val::new(self.call("i64", "zeno_interrupt_wait", waited), Kind::Int))
                }
                _ => Err(cannot(format!("the method `{name}` of an interrupt"), span)),
            },
            // `button.is_low()`: what the pin reads now.
            Kind::InputPin => match (name, args) {
                ("is_high" | "is_low", []) => {
                    let pin = format!("i64 {}", receiver.code);
                    let read = if name == "is_high" { "zeno_input_is_high" } else { "zeno_input_is_low" };
                    Ok(Val::new(self.call("i1", read, pin), Kind::Bool))
                }
                _ => Err(cannot(format!("the method `{name}` of an input pin"), span)),
            },
            Kind::Str => {
                let wrong = || cannot(format!("the str method `{name}` called this way"), span);
                // Two that do not take text: `slice` takes positions,
                // and `code` takes nothing.
                match (name, args) {
                    ("slice", [_]) | ("slice", [_, _]) => {
                        let ends = self.slice_ends(args, span)?;
                        let slice = format!("ptr {}, {ends}, {}", receiver.code, at(span));
                        return Ok(Val::new(self.call("ptr", "zeno_slice", slice), Kind::Str));
                    }
                    ("code", []) => {
                        let text = format!("ptr {}, {}", receiver.code, at(span));
                        return Ok(Val::new(self.call("i64", "zeno_char_code", text), Kind::Int));
                    }
                    _ => {}
                }
                let mut list = format!("ptr {}", receiver.code);
                for arg in args {
                    let val = self.expr(&arg.value)?;
                    if val.kind != Kind::Str || arg.name.is_some() {
                        return Err(wrong());
                    }
                    list += &format!(", ptr {}", val.code);
                }
                let words = Kind::List(Box::new(Kind::Str));
                let (function, returns, kind) = match (name, args.len()) {
                    ("upper", 0) => ("zeno_upper", "ptr", Kind::Str),
                    ("lower", 0) => ("zeno_lower", "ptr", Kind::Str),
                    ("strip", 0) => ("zeno_strip", "ptr", Kind::Str),
                    ("replace", 2) => ("zeno_replace", "ptr", Kind::Str),
                    ("starts_with", 1) => ("zeno_starts_with", "i1", Kind::Bool),
                    ("ends_with", 1) => ("zeno_ends_with", "i1", Kind::Bool),
                    // The runtime answers -1 for "not there".
                    ("find", 1) => {
                        let at = self.call("i64", "zeno_find", list);
                        return Ok(self.found(&at));
                    }
                    ("split", 0) => ("zeno_split_blanks", "ptr", words),
                    ("lines", 0) => ("zeno_lines", "ptr", words),
                    ("split", 1) => {
                        list += &format!(", {}", at(span));
                        ("zeno_split", "ptr", words)
                    }
                    _ => return Err(wrong()),
                };
                // The list of pieces is new, and nobody's yet.
                let val = Val::new(self.call(returns, function, list), kind);
                Ok(if name == "split" || name == "lines" { val.made() } else { val })
            }
            Kind::List(item) => match (name, args) {
                ("map" | "filter" | "reduce", _) => self.over_list(name, &receiver, &item, args, makes_bytes, span),
                // With a function, `sort` is a walk that asks for the
                // key of each item, and then the sorting.
                ("sort", [_]) => self.over_list(name, &receiver, &item, args, false, span),
                ("append", [arg]) if arg.name.is_none() => {
                    let value = self.expr(&arg.value)?;
                    self.stays(&value, span)?;
                    let value = self.keep(value, is_written(&arg.value));
                    let value = self.fit(value, &item, span)?;
                    let slot = self.kept_slot(&value, span)?;
                    self.run(list_fn(&item, "append"), format!("ptr {}, i64 {slot}", receiver.code));
                    Ok(Val::nothing())
                }
                ("pop", []) | ("pop", [Arg { name: None, .. }]) => {
                    let slot = match args.first() {
                        // `pop(2)`: the item at that position.
                        Some(position) => {
                            let position = self.expr(&position.value)?;
                            if position.kind != Kind::Int {
                                return Err(cannot("`pop` with a position that is not an int", span));
                            }
                            let pop = format!("ptr {}, i64 {}, {}", receiver.code, position.code, at(span));
                            self.call("i64", list_fn(&item, "pop_at"), pop)
                        }
                        None => self.call("i64", list_fn(&item, "pop"), format!("ptr {}, {}", receiver.code, at(span))),
                    };
                    // The list is done with it: the statement gets it, and
                    // the list's copy of text, or its box, is freed.
                    self.taken(&item, &slot, span)
                }
                // The list keeps what it is given, as `append` does.
                ("insert", [Arg { name: None, value: position }, arg]) if arg.name.is_none() => {
                    let position = self.expr(position)?;
                    if position.kind != Kind::Int {
                        return Err(cannot("`insert` with a position that is not an int", span));
                    }
                    let value = self.expr(&arg.value)?;
                    self.stays(&value, span)?;
                    let value = self.keep(value, is_written(&arg.value));
                    let value = self.fit(value, &item, span)?;
                    let slot = self.kept_slot(&value, span)?;
                    let insert = format!("ptr {}, i64 {}, i64 {slot}, {}", receiver.code, position.code, at(span));
                    self.run(list_fn(&item, "insert"), insert);
                    Ok(Val::nothing())
                }
                ("reverse", []) => {
                    self.run(list_fn(&item, "reverse"), format!("ptr {}", receiver.code));
                    Ok(Val::nothing())
                }
                // The runtime compares two slots, and has to be told how
                // to read one: 0 a whole number, 1 a float, 2 text.
                ("sort", []) if *item == Kind::Byte => {
                    self.run("zeno_bytes_sort", format!("ptr {}", receiver.code));
                    Ok(Val::nothing())
                }
                ("sort", []) => {
                    let kind = match &*item {
                        Kind::Int => 0,
                        Kind::Float | Kind::F32 => 1,
                        Kind::Str => 2,
                        Kind::U64 => 3,
                        _ => return Err(cannot("`sort` on a list of anything but numbers or text", span)),
                    };
                    self.run("zeno_list_sort", format!("ptr {}, i32 {kind}", receiver.code));
                    Ok(Val::nothing())
                }
                // Items that have one owner (and, with no collector, text,
                // which the list keeps copies of) end with the list.
                // `clear` is the end of each of them: a loop over the
                // items that frees each, and then the call.
                ("clear", []) => {
                    if !self.collector && is_boxed(&item) {
                        let (count, slot) = self.walk_open("item", &receiver.code, &item);
                        let each = self.set(format!("inttoptr i64 {slot} to ptr"));
                        self.run("zeno_free", format!("ptr {each}"));
                        self.count_close(&count);
                    } else if self.drops.owns(&item) || (!self.collector && is_text(&item)) {
                        let (count, slot) = self.walk_open("item", &receiver.code, &item);
                        let each = self.from_slot(&item, &slot, span)?;
                        self.free(&item, &each.code, span)?;
                        self.count_close(&count);
                    }
                    self.run(list_fn(&item, "clear"), format!("ptr {}", receiver.code));
                    Ok(Val::nothing())
                }
                // A byte is looked for by its number: one that is no
                // byte is not there (-1).
                ("find", [arg]) if arg.name.is_none() && *item == Kind::Byte => {
                    let value = self.expr(&arg.value)?;
                    if value.kind != Kind::Int {
                        return Err(cannot("`find` on a list[u8] with anything but a number", span));
                    }
                    let at = self.call("i64", "zeno_bytes_find", format!("ptr {}, i64 {}", receiver.code, value.code));
                    Ok(self.found(&at))
                }
                ("find", [arg]) if arg.name.is_none() => {
                    let same = self.eq_adapter(&item, span)?;
                    let value = self.expr(&arg.value)?;
                    let value = self.coerce(value, &item, span)?;
                    let slot = self.looked_for(&value, span)?;
                    let at = self.call("i64", "zeno_list_find", format!("ptr {}, i64 {slot}, ptr {same}", receiver.code));
                    Ok(self.found(&at))
                }
                ("slice", [_]) | ("slice", [_, _]) => {
                    let ends = self.slice_ends(args, span)?;
                    let mut slice = self.call("ptr", list_fn(&item, "slice"), format!("ptr {}, {ends}, {}", receiver.code, at(span)));
                    if self.keeps_copies(&item) {
                        // With no collector a list of text has its own
                        // copy of each item, and so has the slice.
                        slice = self.keep_items(&item, &slice, span)?;
                    }
                    // The new list is nobody's yet.
                    Ok(Val::new(slice, Kind::List(item.clone())).made())
                }
                ("join", [arg]) if arg.name.is_none() && *item == Kind::Str => {
                    let separator = self.expr(&arg.value)?;
                    if separator.kind != Kind::Str {
                        return Err(cannot("`join` with a separator that is not a str", span));
                    }
                    let join = format!("ptr {}, ptr {}", receiver.code, separator.code);
                    Ok(Val::new(self.call("ptr", "zeno_list_join", join), Kind::Str))
                }
                _ => Err(cannot(format!("the list method `{name}` called this way"), span)),
            },
            Kind::Dict(key, value) => match (name, args) {
                // `get(key)`: the value, or `none`. The runtime says
                // where the key is, or -1, and then gives the value
                // there (zeros for -1). Text that is not there is null
                // already. A number or a bool gets its flag here:
                //
                //   %t3 = call i64 @zeno_dict_find(ptr %t1, i64 %t2)
                //   %t4 = call i64 @zeno_dict_value(ptr %t1, i64 %t3)
                //   %t5 = icmp sge i64 %t3, 0                        ; found?
                //   %t6 = insertvalue { i1, i64 } undef, i1 %t5, 0
                //   %t7 = insertvalue { i1, i64 } %t6, i64 %t4, 1
                ("get", [wanted]) if wanted.name.is_none() => {
                    let wanted = self.expr(&wanted.value)?;
                    let slot = self.to_slot(&wanted, span)?;
                    let at = self.call("i64", "zeno_dict_find", format!("ptr {}, i64 {slot}", receiver.code));
                    let found = self.call("i64", "zeno_dict_value", format!("ptr {}, i64 {at}", receiver.code));
                    let inside = self.from_slot(&value, &found, span)?;
                    let kind = Kind::Optional(value.clone());
                    if is_pointer(&value) {
                        return Ok(self.lend(Val::new(inside.code, kind)));
                    }
                    let present = self.set(format!("icmp sge i64 {at}, 0"));
                    let (pair, ty) = (ir_type(&kind, span)?, ir_type(&value, span)?);
                    let flagged = self.set(format!("insertvalue {pair} undef, i1 {present}, 0"));
                    let whole = self.set(format!("insertvalue {pair} {flagged}, {ty} {}, 1", inside.code));
                    Ok(Val::new(whole, kind))
                }
                // `get(key, instead)`.
                ("get", [wanted, instead]) if wanted.name.is_none() && instead.name.is_none() => {
                    let wanted = self.expr(&wanted.value)?;
                    let instead = self.expr(&instead.value)?;
                    let instead = self.fit(instead, &value, span)?;
                    let slot = self.to_slot(&wanted, span)?;
                    let other = self.to_slot(&instead, span)?;
                    let get = format!("ptr {}, i64 {slot}, i64 {other}", receiver.code);
                    let found = self.call("i64", "zeno_dict_get", get);
                    let val = self.from_slot(&value, &found, span)?;
                    Ok(self.lend(val))
                }
                ("pop", [wanted]) if wanted.name.is_none() => {
                    let wanted = self.expr(&wanted.value)?;
                    let slot = self.to_slot(&wanted, span)?;
                    let pop = format!("ptr {}, i64 {slot}, {}", receiver.code, at(span));
                    let found = self.call("i64", "zeno_dict_pop", pop);
                    let val = self.from_slot(&value, &found, span)?;
                    if self.collector || !is_text(&value) {
                        return Ok(val);
                    }
                    // The dict is done with it: the statement gets it,
                    // and the dict's copy is freed.
                    Ok(Val::new(self.call("ptr", "zeno_text_take", format!("ptr {}", val.code)), val.kind))
                }
                // A new list each time, which is nobody's yet.
                ("keys", []) => {
                    let keys = self.call("ptr", "zeno_dict_keys", format!("ptr {}", receiver.code));
                    Ok(Val::new(keys, Kind::List(key.clone())).made())
                }
                ("values", []) => {
                    let values = self.call("ptr", "zeno_dict_values", format!("ptr {}", receiver.code));
                    Ok(Val::new(values, Kind::List(value.clone())).made())
                }
                ("clear", []) if self.drops.owns(&value) => {
                    let clear = self.dict_helper("clear", &value, span)?;
                    self.ins(format!("call void {clear}(ptr {})", receiver.code));
                    Ok(Val::nothing())
                }
                ("clear", []) => {
                    self.run("zeno_dict_clear", format!("ptr {}", receiver.code));
                    Ok(Val::nothing())
                }
                _ => Err(cannot(format!("the dict method `{name}` called this way"), span)),
            },
            kind => Err(cannot(format!("the method `{name}` of {}", described(&kind)), span)),
        }
    }

    /// The arguments of `slice(from)` or `slice(from, to)`, as the runtime
    /// takes them: where it begins, where it ends, and whether no end
    /// was written (and it goes on to the last item).
    fn slice_ends(&mut self, args: &[Arg], span: Span) -> Result<String, Error> {
        let mut positions = Vec::new();
        for arg in args {
            let position = self.expr(&arg.value)?;
            if position.kind != Kind::Int || arg.name.is_some() {
                return Err(cannot("`slice` with a position that is not an int", span));
            }
            positions.push(position.code);
        }
        Ok(match positions.as_slice() {
            [from] => format!("i64 {from}, i64 0, i1 true"),
            [from, to] => format!("i64 {from}, i64 {to}, i1 false"),
            _ => return Err(cannot("`slice` called this way", span)),
        })
    }

    /// What `find` found, as an `int or none`. The runtime answers with
    /// a position, or -1 when the thing is not there:
    ///
    /// ```text
    ///   %t2 = icmp sge i64 %t1, 0                           ; found?
    ///   %t3 = select i1 %t2, i64 %t1, i64 0
    ///   %t4 = insertvalue { i1, i64 } undef, i1 %t2, 0      ; the flag
    ///   %t5 = insertvalue { i1, i64 } %t4, i64 %t3, 1       ; the position
    /// ```
    fn found(&mut self, at: &str) -> Val {
        let present = self.set(format!("icmp sge i64 {at}, 0"));
        let position = self.set(format!("select i1 {present}, i64 {at}, i64 0"));
        let flagged = self.set(format!("insertvalue {{ i1, i64 }} undef, i1 {present}, 0"));
        let pair = self.set(format!("insertvalue {{ i1, i64 }} {flagged}, i64 {position}, 1"));
        Val::new(pair, Kind::Optional(Box::new(Kind::Int)))
    }

    // ---------------------------------------------------- Objects and enums

    /// `Point(3.0, 4.0)`: a new object.
    fn construct(&mut self, class: &str, args: &[Arg], span: Span) -> Result<Val, Error> {
        let (slots, owned) = (self.classes[class].fields.clone(), self.classes[class].owned_fields.clone());
        let list = self.bind(&slots, &owned, &[], args, span)?;
        let make = self.new_class(class, span)?;
        let object = self.set(format!("call ptr {make}({})", list.join(", ")));
        Ok(Val::new(object, Kind::Class(class.to_string())))
    }

    /// The function that makes an object of a class: ask the collector
    /// for a block the size of the struct, and fill in each field.
    ///
    /// ```text
    /// define internal ptr @zeno.new.Point(double %f0, double %f1) {
    /// entry:
    ///   %t1 = call ptr @zeno_alloc(i64 ptrtoint (ptr getelementptr (%z_Point, ptr null, i32 1) to i64))
    ///   %t2 = getelementptr %z_Point, ptr %t1, i32 0, i32 0
    ///   store double %f0, ptr %t2
    ///   ...
    ///   ret ptr %t1
    /// }
    /// ```
    ///
    /// Like the other functions the compiler writes for itself, it is
    /// written the first time it is needed, and `written` remembers that
    /// it has been.
    fn new_class(&mut self, class: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.new.", class);
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let (ty, fields) = (self.classes[class].ty.clone(), self.classes[class].fields.clone());
        let mut params = Vec::new();
        for (index, (_, kind, _)) in fields.iter().enumerate() {
            params.push(format!("{} %f{index}", ir_type(kind, span)?));
        }
        self.side(format!("define internal ptr {name}({})", params.join(", ")), |emitter| {
            let object = emitter.call("ptr", "zeno_alloc", format!("i64 {}", size_of(&ty)));
            if let Some(number) = emitter.classes[class].number {
                // The number of its class, in front of the fields.
                emitter.ins(format!("store i32 {number}, ptr {object}"));
            }
            for (index, (_, kind, _)) in fields.iter().enumerate() {
                let mut value = format!("%f{index}");
                if !emitter.collector && is_text(kind) {
                    // With no collector the object has a copy of its own.
                    value = emitter.call("ptr", "zeno_text_keep", format!("ptr {value}"));
                }
                emitter.store_field(class, &object, index, &value, span)?;
            }
            emitter.end(format!("ret ptr {object}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The function that shows an object as text: `Point(x=3.0, y=4.0)`.
    fn show_class(&mut self, class: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.show.", class);
        // Noted as written before it is: an object may hold another of
        // its own class, and showing that one calls this same function.
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        if self.classes[class].interface {
            // An object seen as a `Shape` is shown as what it is.
            let classes = self.is_a.get(class).cloned().unwrap_or_default();
            let mut shows = Vec::new();
            for (_, class) in &classes {
                shows.push(self.show_class(class, span)?);
            }
            self.side(format!("define internal ptr {name}(ptr %self)"), |emitter| {
                let blocks = emitter.switch_on_class("%self", &classes);
                for (block, show) in blocks.iter().zip(&shows) {
                    emitter.place(block);
                    let text = emitter.set(format!("call ptr {show}(ptr %self)"));
                    emitter.end(format!("ret ptr {text}"));
                }
                Ok(())
            })?;
            return Ok(name);
        }
        let fields = self.classes[class].fields.clone();
        // A `Stack[int]` prints as `Stack(...)`, as `zeno run` prints it.
        let shown_as = crate::ast::shown_name(class);
        self.side(format!("define internal ptr {name}(ptr %self)"), |emitter| {
            let mut pieces = Vec::new();
            for (index, (field, kind, _)) in fields.iter().enumerate() {
                let label = if index == 0 { format!("{shown_as}({field}=") } else { format!(", {field}=") };
                pieces.push(emitter.string(&label));
                let value = emitter.load_field(class, "%self", index, span)?;
                pieces.push(emitter.repr(&Val::new(value, kind.clone()), span)?);
            }
            if fields.is_empty() {
                pieces.push(emitter.string(&format!("{shown_as}(")));
            }
            pieces.push(emitter.string(")"));
            let shown = emitter.join(&pieces);
            emitter.end(format!("ret ptr {shown}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The function that says whether two objects of a class are equal:
    /// they are when every field is. It stops at the first field that
    /// differs.
    fn same_class(&mut self, class: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.same.", class);
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        if self.classes[class].interface {
            // Two objects seen as a `Shape` are equal if they are of one
            // class, and equal as objects of that class.
            let classes = self.is_a.get(class).cloned().unwrap_or_default();
            let mut sames = Vec::new();
            for (_, class) in &classes {
                sames.push(self.same_class(class, span)?);
            }
            self.side(format!("define internal i1 {name}(ptr %a, ptr %b)"), |emitter| {
                let a = emitter.set("load i32, ptr %a");
                let b = emitter.set("load i32, ptr %b");
                let alike = emitter.set(format!("icmp eq i32 {a}, {b}"));
                emitter.jump_if(&alike, "alike", "differ");
                emitter.place("differ");
                emitter.end("ret i1 false");
                emitter.place("alike");
                let blocks = emitter.switch_on_class("%a", &classes);
                for (block, same) in blocks.iter().zip(&sames) {
                    emitter.place(block);
                    let equal = emitter.set(format!("call i1 {same}(ptr %a, ptr %b)"));
                    emitter.end(format!("ret i1 {equal}"));
                }
                Ok(())
            })?;
            return Ok(name);
        }
        let fields = self.classes[class].fields.clone();
        self.side(format!("define internal i1 {name}(ptr %a, ptr %b)"), |emitter| {
            for (index, (_, kind, _)) in fields.iter().enumerate() {
                let a = emitter.load_field(class, "%a", index, span)?;
                let b = emitter.load_field(class, "%b", index, span)?;
                emitter.same_or_differ(kind, &a, &b, &format!("more{index}"), span)?;
            }
            emitter.end("ret i1 true");
            emitter.place("differ");
            emitter.end("ret i1 false");
            Ok(())
        })?;
        Ok(name)
    }

    /// Inside a function that compares `%a` and `%b`: read field `index`
    /// of both, and leave for `differ` unless the two are equal.
    fn same_field(&mut self, ty: &str, index: usize, kind: &Kind, more: &str, span: Span) -> Result<(), Error> {
        let field_type = ir_type(kind, span)?;
        let (a_at, b_at) = (self.field_at(ty, "%a", index), self.field_at(ty, "%b", index));
        let a = self.set(format!("load {field_type}, ptr {a_at}"));
        let b = self.set(format!("load {field_type}, ptr {b_at}"));
        self.same_or_differ(kind, &a, &b, more, span)
    }

    /// Leave for `differ` unless `a` and `b`, two values of `kind`, are
    /// equal. Otherwise carry on at `more`.
    fn same_or_differ(&mut self, kind: &Kind, a: &str, b: &str, more: &str, span: Span) -> Result<(), Error> {
        let same = self.equal(kind, a, b, span)?;
        self.jump_if(&same, more, "differ");
        self.place(more);
        Ok(())
    }

    /// `Circle(1.0)`: a new value of an enum.
    /// The enum and the place of the case `name`, in a pattern matched
    /// against a value of the kind `there`. A case of a copy of a generic
    /// enum is a case of the copy matched (decision 58.1); any other is
    /// found by its name alone.
    fn case_in(&self, name: &str, there: &Kind) -> Option<(String, usize)> {
        match there {
            Kind::Enum(subject) if self.copied_enums.contains(subject) => {
                let index = self.enums[subject].cases.iter().position(|(case, _)| case == name)?;
                Some((subject.clone(), index))
            }
            _ => self.cases.get(name).cloned(),
        }
    }

    /// The place of the case named at `expr` (a name, or the callee of a
    /// call) in the copy `owner` of a generic enum.
    fn copied_case(&self, owner: &str, expr: &Expr, span: Span) -> Result<usize, Error> {
        let ExprKind::Name(name) = &expr.kind else {
            return Err(cannot("a case of a generic enum written this way", span));
        };
        self.enums[owner]
            .cases
            .iter()
            .position(|(case, _)| case == name)
            .ok_or_else(|| cannot(format!("the case `{name}` of `{owner}`"), span))
    }

    fn construct_case(&mut self, owner: &str, index: usize, args: &[Arg], span: Span) -> Result<Val, Error> {
        let (case, fields) = self.enums[owner].cases[index].clone();
        if fields.is_empty() {
            return Err(cannot(format!("`{case}` called like a function: it carries no values"), span));
        }
        // Which of the values it carries have one owner: the new value
        // takes those over.
        let takes = self.enums[owner].takes[index].clone();
        let slots: Slots = fields.iter().map(|(name, kind)| (name.clone(), kind.clone(), None)).collect();
        let list = self.bind(&slots, &takes, &[], args, span)?;
        // With no collector the value has its own copy of each text.
        let texts = !self.collector;

        let make = named('@', "zeno.new.", &format!("{owner}.{case}"));
        if self.written.insert(make.clone()) {
            let ty = case_type(owner, &case);
            let mut params = Vec::new();
            for (position, (_, kind)) in fields.iter().enumerate() {
                params.push(format!("{} %f{position}", ir_type(kind, span)?));
            }
            self.side(format!("define internal ptr {make}({})", params.join(", ")), |emitter| {
                let value = emitter.call("ptr", "zeno_alloc", format!("i64 {}", size_of(&ty)));
                // The tag is the first thing in the block.
                emitter.ins(format!("store i64 {index}, ptr {value}"));
                for (position, (_, kind)) in fields.iter().enumerate() {
                    let at = emitter.field_at(&ty, &value, position + 1);
                    let carried = match texts && is_text(kind) {
                        true => emitter.call("ptr", "zeno_text_keep", format!("ptr %f{position}")),
                        false => format!("%f{position}"),
                    };
                    emitter.ins(format!("store {} {carried}, ptr {at}", ir_type(kind, span)?));
                }
                emitter.end(format!("ret ptr {value}"));
                Ok(())
            })?;
        }
        let value = self.set(format!("call ptr {make}({})", list.join(", ")));
        Ok(Val::new(value, Kind::Enum(owner.to_string())))
    }

    /// The function that shows an enum value as text: `Idle`, or
    /// `Circle(1.5)`. It reads the tag and goes to the block for that
    /// case. `switch` is a jump with many ways out: one for each number
    /// listed, and one for anything else.
    fn show_enum(&mut self, owner: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.show.", owner);
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let cases = self.enums[owner].cases.clone();
        self.side(format!("define internal ptr {name}(ptr %self)"), |emitter| {
            let tag = emitter.set("load i64, ptr %self");
            let ways: Vec<String> = (0..cases.len()).map(|index| format!("i64 {index}, label %case{index}")).collect();
            emitter.end(format!("switch i64 {tag}, label %other [ {} ]", ways.join(" ")));
            for (index, (case, fields)) in cases.iter().enumerate() {
                emitter.place(&format!("case{index}"));
                // A case of an imported file prints by the name its
                // file gave it: `Dot`, not `shapes.Dot`.
                let shown_as = shown_name(case);
                if fields.is_empty() {
                    let text = emitter.string(shown_as);
                    emitter.end(format!("ret ptr {text}"));
                    continue;
                }
                let ty = case_type(owner, case);
                let mut pieces = Vec::new();
                for (position, (_, kind)) in fields.iter().enumerate() {
                    pieces.push(emitter.string(&if position == 0 { format!("{shown_as}(") } else { ", ".to_string() }));
                    let at = emitter.field_at(&ty, "%self", position + 1);
                    let value = emitter.set(format!("load {}, ptr {at}", ir_type(kind, span)?));
                    pieces.push(emitter.repr(&Val::new(value, kind.clone()), span)?);
                }
                pieces.push(emitter.string(")"));
                let shown = emitter.join(&pieces);
                emitter.end(format!("ret ptr {shown}"));
            }
            emitter.place("other");
            let empty = emitter.string("");
            emitter.end(format!("ret ptr {empty}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The function that says whether two values of an enum are equal:
    /// the same case, carrying equal values.
    fn same_enum(&mut self, owner: &str, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.same.", owner);
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let cases = self.enums[owner].cases.clone();
        self.side(format!("define internal i1 {name}(ptr %a, ptr %b)"), |emitter| {
            let a_tag = emitter.set("load i64, ptr %a");
            let b_tag = emitter.set("load i64, ptr %b");
            let same = emitter.set(format!("icmp eq i64 {a_tag}, {b_tag}"));
            emitter.jump_if(&same, "cases", "differ");
            emitter.place("cases");
            // Only a case that carries something has more to compare.
            let ways: Vec<String> = cases
                .iter()
                .enumerate()
                .filter(|(_, (_, fields))| !fields.is_empty())
                .map(|(index, _)| format!("i64 {index}, label %case{index}"))
                .collect();
            emitter.end(format!("switch i64 {a_tag}, label %alike [ {} ]", ways.join(" ")));
            for (index, (case, fields)) in cases.iter().enumerate() {
                if fields.is_empty() {
                    continue;
                }
                emitter.place(&format!("case{index}"));
                let ty = case_type(owner, case);
                for (position, (_, kind)) in fields.iter().enumerate() {
                    emitter.same_field(&ty, position + 1, kind, &format!("more{index}.{position}"), span)?;
                }
                emitter.end("ret i1 true");
            }
            emitter.place("alike");
            emitter.end("ret i1 true");
            emitter.place("differ");
            emitter.end("ret i1 false");
            Ok(())
        })?;
        Ok(name)
    }

    // --------------------------------------------------- Functions as values

    /// A function of the program used as a value: `apply(double, 3)`.
    ///
    /// A function value is the address of a small block: the address of
    /// its code, its name, and then whatever variables it took with it
    /// (see `closure`). Every function value is called the same way, with
    /// that block as a hidden first argument. An ordinary function does
    /// not take that argument, so it gets a small wrapper that does, and
    /// one fixed block that points at the wrapper.
    ///
    /// In a firmware build the wrapper of a function that can raise gives
    /// back an `i1`, as the function does. A function that raises
    /// nothing, put where a function type says `raises`, gets a second
    /// wrapper of that kind, which always gives back 1 (decision 56.5).
    fn function_value(&mut self, name: &str, span: Span) -> Result<Val, Error> {
        let entry = self.functions[name].clone();
        let mut kind = entry.kind.clone();
        kind.raises = match self.firmware {
            true => entry.decl.raises.clone(),
            false => Vec::new(),
        };
        // A raising function that raises nothing.
        let made = match (kind.raises.is_empty(), self.types.made_raising.get(&spot(self.within, span))) {
            (true, Some(said)) if self.firmware => {
                kind.raises = vec![said.clone()];
                true
            }
            _ => false,
        };
        let raising = self.raising(&kind);
        // A function written inside a function has a number of its own
        // (decision 56.16), which its wrappers carry too.
        let key = match entry.name.strip_prefix("@zeno.inner.").or_else(|| entry.name.strip_prefix("@\"zeno.inner.")) {
            Some(rest) => format!("inner.{}.{name}", rest.split('.').next().unwrap_or_default()),
            None => name.to_string(),
        };
        let (shown, name) = (name, key.as_str());
        // The second wrapper has a prefix of its own, which no name of
        // the program can make.
        let (value, wrap) = match made {
            true => (named('@', "zeno.raising.value.", name), named('@', "zeno.raising.wrap.", name)),
            false => (named('@', "zeno.value.", name), named('@', "zeno.wrap.", name)),
        };
        if self.written.insert(value.clone()) {
            let mut params = vec!["ptr %self".to_string()];
            let mut args = Vec::new();
            for (index, param) in kind.params.iter().enumerate() {
                let ty = ir_type(param, span)?;
                params.push(format!("{ty} %a{index}"));
                args.push(format!("{ty} %a{index}"));
            }
            params.extend(gives_params(&kind.returns));
            let gives = kind.returns != Kind::None;
            if raising && gives {
                params.push("ptr %result".to_string());
            }
            let returns = return_type(&kind.returns, span)?;
            let first_line = match raising {
                true => format!("define internal zeroext i1 {wrap}({})", params.join(", ")),
                false => format!("define internal {returns} {wrap}({})", params.join(", ")),
            };
            self.side(first_line, |emitter| {
                // The function can raise: what it says is what the
                // wrapper says, and its answer goes straight through.
                if raising && !made {
                    args.extend(gives_params(&kind.returns));
                    if gives {
                        args.push("ptr %result".to_string());
                    }
                    let worked = emitter.set(format!("call zeroext i1 {}({})", entry.name, args.join(", ")));
                    emitter.end(format!("ret i1 {worked}"));
                    return Ok(());
                }
                // The wrapper's own places for later values, kept apart
                // from those of the call this value is an argument of.
                let outer = emitter.outs.replace(gives_params(&kind.returns));
                let result = emitter.call_entry(&entry, args, span);
                emitter.outs = outer;
                let result = result?;
                match (raising, result.kind) {
                    (false, Kind::None) => emitter.end("ret void"),
                    (false, _) => emitter.end(format!("ret {returns} {}", result.code)),
                    (true, Kind::None) => emitter.end("ret i1 true"),
                    (true, _) => {
                        emitter.ins(format!("store {returns} {}, ptr %result", result.code));
                        emitter.end("ret i1 true");
                    }
                }
                Ok(())
            })?;
            let text = self.string(shown);
            self.globals.push(match self.collector {
                true => format!("{value} = internal constant {{ ptr, ptr }} {{ ptr {wrap}, ptr {text} }}"),
                // With no collector a function value says what frees
                // it. Nothing frees this one: it is not in a block.
                false => {
                    format!("{value} = internal constant {{ ptr, ptr, ptr }} {{ ptr {wrap}, ptr {text}, ptr null }}")
                }
            });
        }
        Ok(Val::new(value, Kind::Function(Box::new(kind))))
    }

    /// A function written where a value goes: `function(n) { return n * k }`.
    ///
    /// It becomes a function of its own in the IR. The variables it uses
    /// from the function around it are packed into the block that the
    /// function value is, right after the two fields every function value
    /// has. That pair, some code and the variables it took with it, is
    /// called a closure.
    ///
    /// ```text
    ///   %t1 = call ptr @zeno_alloc(i64 ...)          ; { code, name, k }
    ///   store ptr @zeno.fn.1, ptr %t1
    ///   ...
    /// define internal i64 @zeno.fn.1(ptr %self, i64 %arg.n) {
    /// entry:
    ///   %t1 = getelementptr { ptr, ptr, i64 }, ptr %self, i32 0, i32 2
    ///   %t2 = load i64, ptr %t1                       ; k
    /// ```
    ///
    /// A `let` is copied into the block: it never changes, so a copy is
    /// as good as the original. A `var` that such a function uses already
    /// lives in a box of its own (see `declare`), and the block holds the
    /// box's address, so both functions see each other's changes.
    /// `take slot.task`, `take tasks[i]`: what the place holds, which is
    /// left holding `none` (decision 56.10). Nothing is freed: whoever
    /// receives the value owns it, and if nobody does, the statement does
    /// (`belongs`). Text that a build with no collector kept there leaves
    /// its keeper as `pop()` makes it leave a list.
    fn take_out(&mut self, place: &Expr) -> Result<Val, Error> {
        let span = place.span;
        match &place.kind {
            ExprKind::Field { object, name, .. } => {
                let object = self.expr(object)?;
                let field = match &object.kind {
                    Kind::Class(class) => self.field_of(class, name).map(|field| (class.clone(), field)),
                    _ => None,
                };
                let Some((class, (_, index, kind))) = field else {
                    return Err(cannot("`take` in front of this", span));
                };
                let Kind::Optional(inner) = &kind else {
                    return Err(cannot("`take` in front of this", span));
                };
                let empty = nothing(inner, span)?;
                let mut code = self.load_field(&class, &object.code, index, span)?;
                if !self.collector && is_text(&kind) {
                    code = self.call("ptr", "zeno_text_take", format!("ptr {code}"));
                }
                self.store_field(&class, &object.code, index, empty, span)?;
                Ok(Val::new(code, kind))
            }
            ExprKind::Index { object, index } => {
                let list = self.expr(object)?;
                let index = self.expr(index)?;
                let Kind::List(item) = list.kind.clone() else {
                    return Err(cannot("`take` in front of this", span));
                };
                let Kind::Optional(inner) = &*item else {
                    return Err(cannot("`take` in front of this", span));
                };
                let none = Val::new(nothing(inner, span)?.to_string(), (*item).clone());
                let at = self.list_place(&list.code, &index.code, &item, span);
                let slot = self.load_item(&at, &item);
                let val = self.taken(&item, &slot, span)?;
                let empty = self.to_slot(&none, span)?;
                self.store_item(&empty, &at, &item);
                Ok(val)
            }
            // The parser lets nothing else through.
            _ => Err(cannot("`take` in front of this", span)),
        }
    }

    /// `counter.bump` used as a value is the function `function(n: int)
    /// -> int { return counter.bump(n) }`, which the checker wrote out
    /// (decision 56.9). The method is bound to the object `counter`
    /// holds now, as in `zeno run`, so the function is offered the value
    /// read here, and not the variable's slot or a global, unless it
    /// takes the object, which moves it out of the slot itself.
    fn method_value(&mut self, object: &Expr, method: &str, written: &Arc<Expr>) -> Result<Val, Error> {
        let (ExprKind::Function(decl), ExprKind::Name(name)) = (&written.kind, &object.kind) else {
            return Err(cannot("this method used as a value", written.span));
        };
        let takes = self.types.taken.get(&spot(self.within, written.span)).is_some_and(|taken| taken.contains(name));
        if takes {
            return self.closure_named(decl, method, written.span);
        }
        let now = self.expr(object)?;
        let copy = Variable { place: Place::Value(now.code), kind: now.kind, open: false, owner: false, inner: None };
        self.scopes.push(HashMap::from([(name.clone(), copy)]));
        let made = self.closure_named(decl, method, written.span);
        self.scopes.pop();
        made
    }

    fn closure(&mut self, decl: &Arc<Function>, span: Span) -> Result<Val, Error> {
        self.closure_named(decl, &decl.name, span)
    }

    /// A function written where a value goes, under the name `print`
    /// shows: none, or the method's for one used as a value.
    fn closure_named(&mut self, decl: &Arc<Function>, shown: &str, span: Span) -> Result<Val, Error> {
        self.closure_made(decl, shown, None, span)
    }

    /// The same, for a function with a name written inside a function
    /// (`own`, decision 56.16): inside its body its name is the function
    /// itself, and the checker's notes for its body are under its own
    /// declaration.
    fn closure_made(&mut self, decl: &Arc<Function>, shown: &str, own: Option<&Arc<Function>>, span: Span) -> Result<Val, Error> {
        let Some(kind) = self.types.function(decl) else {
            return Err(cannot("this function", span));
        };
        let owned: Vec<bool> = kind.params.iter().map(|param| matches!(param, Kind::Owned(_))).collect();
        let mut kind = FunctionKind {
            params: kind.params.iter().map(shared).collect(),
            returns: shared(&kind.returns),
            raises: kind.raises.clone(),
        };
        // In a firmware build one that can raise gives back an `i1`, as a
        // function of the program that can raise does. So does one that
        // raises nothing, put where a function type says `raises`
        // (decision 56.5).
        kind.raises = match (self.firmware, self.types.made_raising.get(&spot(self.within, span))) {
            (false, _) => Vec::new(),
            (true, Some(said)) if decl.raises.is_empty() => vec![said.clone()],
            (true, _) => decl.raises.clone(),
        };
        let fallible = self.raising(&kind);
        self.closures += 1;
        let code_name = format!("@zeno.fn.{}", self.closures);
        let drop_name = format!("@zeno.fn.drop.{}", self.closures);

        // The variables of the functions around this spot that the new
        // function mentions. The top of the file is not among them: its
        // variables are globals, which every function can reach.
        let mut mentioned = HashSet::new();
        for statement in &decl.body {
            emit_c::names_in_functions(statement, true, &mut mentioned);
        }
        let mut mentioned: Vec<String> = mentioned.into_iter().collect();
        mentioned.sort();
        // A function that is kept takes the values with one owner that it
        // uses (decision 56.8): the variable out here holds nothing from
        // this line on, and the block frees them when it goes.
        let takes = self.types.taken.get(&spot(self.within, span)).cloned().unwrap_or_default();
        let mut taken = Vec::new();
        let mut owns = Vec::new();
        for name in mentioned {
            if decl.params.iter().any(|param| param.name == name) || (own.is_some() && name == decl.name) {
                continue;
            }
            if let Some(variable) = self.scopes[1..].iter().rev().find_map(|scope| scope.get(&name)) {
                let mut variable = variable.clone();
                // A function written where a value goes may outlive the
                // block that owns a function with a name it would take,
                // unless there is a collector.
                variable.inner = variable.inner.map(|(decl, usable)| (decl, usable && (self.collector || own.is_some())));
                if takes.contains(&name) {
                    let Place::Slot(slot) = &variable.place else {
                        return Err(cannot(format!("a function that takes `{name}` from outside it"), span));
                    };
                    let ty = ir_type(&variable.kind, span)?;
                    let value = self.load(ty, slot);
                    self.store(ty, "null", slot);
                    variable.place = Place::Value(value);
                    variable.owner = false;
                    owns.push(taken.len());
                } else if !self.collector {
                    // With no collector nothing is shared. The function
                    // takes what the variable holds at this moment: a
                    // number, or the address of a list or an object that
                    // it will only look at.
                    if let Place::Slot(slot) = &variable.place {
                        let ty = ir_type(&variable.kind, span)?;
                        variable.place = Place::Value(self.load(ty, slot));
                    }
                    variable.owner = false;
                }
                taken.push((name, variable));
            }
        }
        // Every function value begins with its code and its name. With
        // no collector, what frees it comes third.
        let mut inside = vec!["ptr", "ptr"];
        if !self.collector {
            inside.push("ptr");
        }
        let first = inside.len();
        for (name, variable) in &taken {
            inside.push(match &variable.place {
                Place::Value(_) => ir_type(&variable.kind, span)?,
                Place::Slot(slot) if self.code.on_stack.contains(slot) => {
                    return Err(cannot(format!("a function that uses `{name}` from outside it"), span))
                }
                Place::Slot(_) => "ptr",
            });
        }
        let block = format!("{{ {} }}", inside.join(", "));

        // The value itself is made here, each time this spot is reached:
        // two function values are equal only if they are the very same
        // one, as in `zeno run`.
        let made = self.call("ptr", "zeno_alloc", format!("i64 {}", size_of(&block)));
        self.ins(format!("store ptr {code_name}, ptr {made}"));
        let name_at = self.field_at(&block, &made, 1);
        let name = self.string(shown);
        self.ins(format!("store ptr {name}, ptr {name_at}"));
        // With no collector the function has its own copy of any text
        // it uses, which goes when the block goes.
        let texts: Vec<usize> = taken
            .iter()
            .enumerate()
            .filter(|(index, (_, variable))| !self.collector && is_text(&variable.kind) && !owns.contains(index))
            .map(|(index, _)| index + first)
            .collect();
        for (index, (_, variable)) in taken.iter().enumerate() {
            let at = self.field_at(&block, &made, index + first);
            match &variable.place {
                Place::Value(code) if texts.contains(&(index + first)) => {
                    let kept = self.call("ptr", "zeno_text_keep", format!("ptr {code}"));
                    self.ins(format!("store ptr {kept}, ptr {at}"));
                }
                Place::Value(code) => self.ins(format!("store {} {code}, ptr {at}", inside[index + first])),
                Place::Slot(boxed) => self.ins(format!("store ptr {boxed}, ptr {at}")),
            }
        }
        if !self.collector {
            let drop_at = self.field_at(&block, &made, 2);
            if texts.is_empty() && owns.is_empty() {
                self.uses("zeno_closure_release");
                self.ins(format!("store ptr @zeno_closure_release, ptr {drop_at}"));
            } else {
                let block = block.clone();
                let owned: Vec<(usize, Kind)> = owns.iter().map(|index| (index + first, taken[*index].1.kind.clone())).collect();
                self.side(format!("define internal void {drop_name}(ptr %self)"), |emitter| {
                    for index in &texts {
                        let at = emitter.field_at(&block, "%self", *index);
                        let text = emitter.set(format!("load ptr, ptr {at}"));
                        emitter.run("zeno_text_drop", format!("ptr {text}"));
                    }
                    // What it took, by the kind of each.
                    for (index, kind) in &owned {
                        let at = emitter.field_at(&block, "%self", *index);
                        let value = emitter.set(format!("load ptr, ptr {at}"));
                        emitter.free(kind, &value, span)?;
                    }
                    emitter.run("zeno_free", "ptr %self");
                    emitter.end("ret void");
                    Ok(())
                })?;
                self.ins(format!("store ptr {drop_name}, ptr {drop_at}"));
            }
        }

        // Set aside the function being written, and write the new one.
        let outer = std::mem::replace(&mut self.code, Code::of(&decl.body, kind.returns.clone(), true, !self.firmware));
        self.code.fallible = fallible;
        let mut outside = std::mem::take(&mut self.scopes);
        let globals = outside.remove(0);
        // It begins by taking its variables out of the block.
        let mut brought = HashMap::new();
        for (index, (name, variable)) in taken.iter().enumerate() {
            let at = self.field_at(&block, "%self", index + first);
            let place = match &variable.place {
                Place::Value(_) => Place::Value(self.set(format!("load {}, ptr {at}", inside[index + first]))),
                Place::Slot(_) => Place::Slot(self.set(format!("load ptr, ptr {at}"))),
            };
            brought.insert(name.clone(), Variable { place, kind: variable.kind.clone(), open: variable.open, owner: false, inner: variable.inner.clone() });
        }
        if let Some(own) = own {
            let itself = Variable {
                place: Place::Value("%self".to_string()),
                kind: Kind::Function(Box::new(kind.clone())),
                open: false,
                owner: false,
                inner: Some((own.clone(), true)),
            };
            brought.insert(decl.name.clone(), itself);
        }
        let (mut params, list) = self.parameters(decl, &kind, span)?;
        // A parameter that takes over a value is its owner, as in a
        // function of the program: see `function`.
        for (index, param) in decl.params.iter().enumerate() {
            if owned.get(index) == Some(&true) {
                let kind = kind.params[index].clone();
                let ty = ir_type(&kind, span)?;
                let slot = self.stack_slot(&param.name, ty);
                self.store(ty, &named('%', "arg.", &param.name), &slot);
                params.insert(param.name.clone(), Variable { place: Place::Slot(slot), kind, open: false, owner: true, inner: None });
            }
        }
        self.scopes = vec![globals, brought, params];
        let within = match own {
            Some(own) => std::mem::replace(&mut self.within, Arc::as_ptr(own) as usize),
            None => self.within,
        };
        let mut done = self.block(&decl.body);
        self.within = within;
        if done.is_ok() && !self.code.ended {
            // The end of the body: free what the parameters still own.
            done = self.free_owned(1);
        }

        // Put everything back as it was.
        outside.insert(0, self.scopes.remove(0));
        self.scopes = outside;
        done?;
        let mut all = vec!["ptr %self".to_string()];
        all.extend(list);
        all.extend(gives_params(&kind.returns));
        let mut returns = return_type(&kind.returns, span)?;
        if fallible {
            if kind.returns != Kind::None {
                all.push("ptr %result".to_string());
            }
            returns = "zeroext i1";
        }
        let text = self.finish(format!("define internal {returns} {code_name}({})", all.join(", ")));
        self.code = outer;
        self.definitions.push(text);
        Ok(Val::new(made, Kind::Function(Box::new(kind))))
    }

    /// A call to a function held in a variable, or to anything else that
    /// is a function value: `f(3)`, `make_adder(2)(3)`, `handlers[0]()`.
    fn call_value(&mut self, function: Val, args: &[Arg], span: Span) -> Result<Val, Error> {
        let Kind::Function(kind) = function.kind.clone() else {
            return Err(cannot(format!("a call to {}", described(&function.kind)), span));
        };
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(cannot("named arguments to a function held in a variable", span));
        }
        if args.len() != kind.params.len() {
            return Err(cannot("a call with this many arguments", span));
        }
        // What may be given to each parameter, and what it does with
        // it, is the checker's to say: the function's type tells it.
        let mut vals = Vec::new();
        for arg in args {
            vals.push(self.expr(&arg.value)?);
        }
        self.invoke(&function.code, &kind, vals, span)
    }

    /// Call the function value at `function` with these values. The
    /// address of its code is the first thing in its block, and the block
    /// itself goes along as the first argument.
    fn invoke(&mut self, function: &str, kind: &FunctionKind, vals: Vec<Val>, span: Span) -> Result<Val, Error> {
        let mut list = vec![format!("ptr {function}")];
        for (val, param) in vals.into_iter().zip(&kind.params) {
            let val = self.fit(val, param, span)?;
            list.push(format!("{} {}", ir_type(param, span)?, val.code));
        }
        list.extend(self.outs_for(&kind.returns, span)?);
        let code = self.set(format!("load ptr, ptr {function}"));
        // In a firmware build a function that can raise gives back an
        // `i1`, and is tested as a call of one by its name.
        if self.raising(kind) {
            let call = format!("call zeroext i1 {code}({}", list.join(", "));
            return self.checked_call(call, &kind.returns, &kind.raises, span);
        }
        let call = format!("call {} {code}({})", return_type(&kind.returns, span)?, list.join(", "));
        Ok(match &kind.returns {
            Kind::None => {
                self.ins(call);
                Val::nothing()
            }
            returns => {
                let val = Val::new(self.set(call), returns.clone());
                self.came_back(val)
            }
        })
    }

    /// `items.map(f)`, `items.filter(f)`, `items.reduce(f, start)`: each
    /// is a loop that calls `f` on every item.
    ///
    /// They walk the list as it was when they began, as `zeno run` does,
    /// so `f` may change the list without confusing the loop.
    fn over_list(&mut self, name: &str, list: &Val, item: &Kind, args: &[Arg], bytes: bool, span: Span) -> Result<Val, Error> {
        let wrong = || cannot(format!("the list method `{name}` called this way"), span);
        if args.iter().any(|arg| arg.name.is_some()) {
            return Err(wrong());
        }
        let mut vals = Vec::new();
        for arg in args {
            vals.push(self.expr(&arg.value)?);
        }
        let takes = if name == "reduce" { 2 } else { 1 };
        let kind = match vals.first().map(|function| &function.kind) {
            Some(Kind::Function(kind)) if kind.params.len() == takes && vals.len() == takes => (**kind).clone(),
            _ => return Err(wrong()),
        };
        let function = vals[0].code.clone();

        // `reduce` starts from its second argument. The running total
        // changes on every trip, so it has a slot.
        let total = match vals.get(1) {
            Some(start) => {
                let start = self.fit(start.clone(), &kind.returns, span)?;
                let ty = ir_type(&kind.returns, span)?;
                let n = self.number();
                let total = self.slot(format!("%total.{n}"), ty);
                self.store(ty, &start.code, &total);
                Some((total, ty))
            }
            None => None,
        };
        // With no collector a list of text is walked as a `for` loop
        // walks one: a copy with copies of the items, which the function
        // that is called cannot pull away.
        let texts = !self.collector && is_text(item);
        let copy = if texts { "zeno_list_copy_text" } else { list_fn(item, "copy") };
        // A list that the statement owns is out of every function's
        // reach, so it is walked as it is.
        let own = list.passing;
        // And a list of numbers that may be `none`, with boxes of its own
        // (56.14).
        let boxes = !self.collector && is_boxed(item);
        let items = match (own, boxes) {
            (true, _) => list.code.clone(),
            (false, true) => {
                let copy = self.call("ptr", copy, format!("ptr {}", list.code));
                self.keep_items(item, &copy, span)?
            }
            (false, false) => self.call("ptr", copy, format!("ptr {}", list.code)),
        };
        // How the runtime reads a key that `sort` orders by: 0 a whole
        // number, 1 a float, 2 text.
        let by = match (name, &kind.returns) {
            ("sort", Kind::Int) => 0,
            ("sort", Kind::Float | Kind::F32) => 1,
            ("sort", Kind::Str) => 2,
            ("sort", Kind::U64) => 3,
            ("sort", _) => {
                return Err(cannot("`sort` with a function that returns anything but a number or text", span))
            }
            _ => 0,
        };
        // What `map` makes: a list[u8] keeps one byte to an item.
        let made = if bytes && name == "map" { Kind::Byte } else { kind.returns.clone() };
        let out = match name {
            "map" | "sort" => {
                let len = self.set(format!("load i64, ptr {items}"));
                self.call("ptr", list_fn(&made, "new"), format!("i64 {len}"))
            }
            "filter" if kind.returns != Kind::Bool => {
                return Err(cannot("`filter` with a function that does not return a bool", span))
            }
            "filter" => self.call("ptr", list_fn(item, "new"), "i64 0"),
            _ => String::new(),
        };

        // With no collector the text each call of `map` gives back is in
        // the text room. The new list takes a copy of its own, and the
        // room is given back before the next call.
        let mark = match matches!(name, "map" | "sort") && !self.collector && is_text(&kind.returns) {
            true => {
                self.code.marks += 1;
                let mark = format!("%mark.{}", self.code.marks);
                self.ins(format!("{mark} = load i64, ptr @zeno_text_top"));
                Some(mark)
            }
            false => None,
        };
        // In a firmware build with no collector a function that can
        // raise may fail on any item. Until the loop is over, the copy
        // that is walked and the list being made are held, so that a
        // failure frees them on its way out. Only text in a copy is the
        // copy's own.
        let mut held = Vec::new();
        if self.raising(&kind) && !self.collector {
            if !own {
                let copied = Kind::List(Box::new(if texts || boxes { item.clone() } else { Kind::None }));
                held.push(self.hold(copied, &items));
            }
            let making = match name {
                "map" => Some(made.clone()),
                "sort" => Some(kind.returns.clone()),
                "filter" if texts || boxes => Some(item.clone()),
                "filter" => Some(Kind::None),
                _ => None,
            };
            if let Some(making) = making {
                held.push(self.hold(Kind::List(Box::new(making)), &out));
            }
        }
        let (count, slot) = self.walk_open(name, &items, item);
        let each = self.from_slot(item, &slot, span)?;
        let result = match (name, &total) {
            ("map" | "sort", _) => {
                let mut made = self.invoke(&function, &kind, vec![each], span)?;
                if mark.is_some() {
                    made.code = self.call("ptr", "zeno_text_keep", format!("ptr {}", made.code));
                }
                let into = if bytes && name == "map" { Kind::Byte } else { kind.returns.clone() };
                let made = self.coerce(made, &into, span)?;
                let slot = self.kept_slot(&made, span)?;
                self.run(list_fn(&into, "append"), format!("ptr {out}, i64 {slot}"));
                if let Some(mark) = &mark {
                    self.text_back(mark);
                }
                Val::new(out.clone(), Kind::List(Box::new(into))).made()
            }
            ("filter", _) => {
                let wanted = self.invoke(&function, &kind, vec![each.clone()], span)?;
                let keep = format!("keep{}", self.number());
                self.jump_if(&wanted.code, &keep, &count.next);
                self.place(&keep);
                // With no collector the new list has its own copy of each
                // text it takes.
                let taken = match texts {
                    true => {
                        let kept = self.call("ptr", "zeno_text_keep", format!("ptr {}", each.code));
                        self.to_slot(&Val::new(kept, each.kind.clone()), span)?
                    }
                    false if boxes => self.kept_slot(&each, span)?,
                    false => slot,
                };
                self.run(list_fn(item, "append"), format!("ptr {out}, i64 {taken}"));
                Val::new(out.clone(), Kind::List(Box::new(item.clone()))).made()
            }
            (_, Some((total, ty))) => {
                let so_far = Val::new(self.load(ty, total), kind.returns.clone());
                let next = self.invoke(&function, &kind, vec![so_far, each], span)?;
                self.store(ty, &next.code, total);
                Val::nothing()
            }
            _ => return Err(wrong()),
        };
        self.count_close(&count);
        for key in &held {
            self.let_go(key);
        }
        // `people.sort(age_of)`: `out` has the key of each item. The
        // runtime puts the items in the order of their keys, and the
        // keys are done with.
        let result = match name {
            "sort" => {
                let sorted = if own { &items } else { &list.code };
                self.run(list_fn(item, "sort_by"), format!("ptr {sorted}, ptr {out}, i32 {by}, {}", at(span)));
                self.run(if mark.is_some() { "zeno_list_drop_text" } else { "zeno_list_drop" }, format!("ptr {out}"));
                Val::nothing()
            }
            _ => result,
        };
        if !own {
            // The copy that was walked was nobody else's.
            // Its items, if they have one owner each, are still the
            // first list's.
            let drop = match (texts, boxes) {
                (true, _) => "zeno_list_drop_text",
                (_, true) => "zeno.list.drop.boxes",
                _ => "zeno_list_drop",
            };
            self.run(drop, format!("ptr {items}"));
        }
        Ok(match total {
            Some((total, ty)) => Val::new(self.load(ty, &total), kind.returns.clone()),
            None => result,
        })
    }

    // ------------------------------------------------- Showing and comparing

    /// Several pieces of text made into one.
    fn join(&mut self, pieces: &[String]) -> String {
        self.uses("zeno_join");
        let list: Vec<String> = pieces.iter().map(|piece| format!("ptr {piece}")).collect();
        self.set(format!("call ptr (i32, ...) @zeno_join(i32 {}, {})", pieces.len(), list.join(", ")))
    }

    /// The value as text, for `print`, `str` and f-strings.
    fn text(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        // Anything but text has to be turned into text, which builds some.
        if val.kind != Kind::Str && !self.collector {
            self.code.built = true;
        }
        Ok(match &val.kind {
            Kind::Str => val.code.clone(),
            Kind::Int => self.call("ptr", "zeno_str_int", format!("i64 {}", val.code)),
            Kind::U64 => self.call("ptr", "zeno_str_u64", format!("i64 {}", val.code)),
            Kind::Float => self.call("ptr", "zeno_str_float", format!("double {}", val.code)),
            Kind::F32 => {
                let near = self.set(format!("fptrunc double {} to float", val.code));
                self.call("ptr", "zeno_str_f32", format!("float {near}"))
            }
            Kind::Bool => self.call("ptr", "zeno_str_bool", format!("i1 {}", val.code)),
            Kind::Pin => self.call("ptr", "zeno_str_pin", format!("i64 {}", val.code)),
            Kind::InputPin => self.call("ptr", "zeno_str_input_pin", format!("i64 {}", val.code)),
            Kind::Serial => self.call("ptr", "zeno_str_serial", format!("i64 {}", val.code)),
            Kind::AnalogPin => self.call("ptr", "zeno_str_analog_pin", format!("i64 {}", val.code)),
            Kind::PwmPin => self.call("ptr", "zeno_str_pwm_pin", format!("i64 {}", val.code)),
            Kind::Interrupt => self.call("ptr", "zeno_str_interrupt", format!("i64 {}", val.code)),
            Kind::Ticker => self.call("ptr", "zeno_ticker_str", format!("i64 {}", val.code)),
            // Where a pointer points is different on every run, so it
            // is not shown.
            Kind::Pointer(pointee) => self.string(&format!("<pointer[{}]>", pointee.name())),
            Kind::Register(_) => self.call("ptr", "zeno_str_register", format!("i64 {}", val.code)),
            Kind::Quantity(unit) => {
                let unit = self.string(unit);
                self.call("ptr", "zeno_str_quantity", format!("double {}, ptr {unit}", val.code))
            }
            Kind::Class(name) => {
                let show = self.show_class(name, span)?;
                self.set(format!("call ptr {show}(ptr {})", val.code))
            }
            Kind::Enum(name) => {
                let show = self.show_enum(name, span)?;
                self.set(format!("call ptr {show}(ptr {})", val.code))
            }
            // The runtime knows how to lay a list out, and is handed a
            // function that shows one item.
            Kind::List(item) if **item == Kind::Byte => {
                self.call("ptr", "zeno_bytes_str", format!("ptr {}", val.code))
            }
            Kind::List(item) => {
                let show = self.repr_adapter(item, span)?;
                self.call("ptr", "zeno_list_str", format!("ptr {}, ptr {show}", val.code))
            }
            // A dict of numbers, bools or text knows what it holds, and
            // shows itself. For any other value it is handed a function
            // that shows one, as a list is.
            Kind::Dict(_, value) if !emit_c::plain_value(value) && !matches!(**value, Kind::Other(_)) => {
                let show = self.repr_adapter(value, span)?;
                self.call("ptr", "zeno_dict_str_with", format!("ptr {}, ptr {show}", val.code))
            }
            Kind::Dict(..) => self.call("ptr", "zeno_dict_str", format!("ptr {}", val.code)),
            Kind::Error => {
                let at = self.field_at(ERROR, &val.code, 1);
                self.set(format!("load ptr, ptr {at}"))
            }
            Kind::Function(_) => self.call("ptr", "zeno_show_function", format!("ptr {}", val.code)),
            Kind::Optional(inner) => {
                let show = self.optional_text(inner, false, span)?;
                self.set(format!("call ptr {show}({} {})", ir_type(&val.kind, span)?, val.code))
            }
            // `none` itself, or a call that gave nothing back.
            Kind::None => self.string("none"),
            kind => return Err(cannot(format!("printing {}", described(kind)), span)),
        })
    }

    /// The text for a value inside a list or an object. Only a str
    /// differs: there it is shown in quotes.
    fn repr(&mut self, val: &Val, span: Span) -> Result<String, Error> {
        if !self.collector {
            self.code.built = true;
        }
        match &val.kind {
            Kind::Str => Ok(self.call("ptr", "zeno_repr_str", format!("ptr {}", val.code))),
            Kind::Optional(inner) => {
                let show = self.optional_text(inner, true, span)?;
                Ok(self.set(format!("call ptr {show}({} {})", ir_type(&val.kind, span)?, val.code)))
            }
            _ => self.text(val, span),
        }
    }

    /// One bit that is set when two values of this kind are equal.
    /// Objects, enum values and lists are equal when their contents are.
    fn equal(&mut self, kind: &Kind, a: &str, b: &str, span: Span) -> Result<String, Error> {
        Ok(match kind {
            Kind::Int | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Register(_) => self.set(format!("icmp eq i64 {a}, {b}")),
            Kind::Byte | Kind::U64 => self.set(format!("icmp eq i64 {a}, {b}")),
            // Two pointers are equal when they point at the same item.
            Kind::Pointer(_) => self.set(format!("icmp eq ptr {a}, {b}")),
            Kind::Float | Kind::F32 | Kind::Quantity(_) => self.set(format!("fcmp oeq double {a}, {b}")),
            Kind::Bool => self.set(format!("icmp eq i1 {a}, {b}")),
            Kind::Str => {
                let order = self.call("i32", "strcmp", format!("ptr {a}, ptr {b}"));
                self.set(format!("icmp eq i32 {order}, 0"))
            }
            Kind::Class(name) => {
                let same = self.same_class(name, span)?;
                self.set(format!("call i1 {same}(ptr {a}, ptr {b})"))
            }
            Kind::Enum(name) => {
                let same = self.same_enum(name, span)?;
                self.set(format!("call i1 {same}(ptr {a}, ptr {b})"))
            }
            Kind::List(item) if **item == Kind::Byte => self.call("i1", "zeno_bytes_eq", format!("ptr {a}, ptr {b}")),
            Kind::List(item) => {
                let same = self.eq_adapter(item, span)?;
                self.call("i1", "zeno_list_eq", format!("ptr {a}, ptr {b}, ptr {same}"))
            }
            Kind::Dict(_, value) if !emit_c::plain_value(value) && !matches!(**value, Kind::Other(_)) => {
                let same = self.eq_adapter(value, span)?;
                self.call("i1", "zeno_dict_eq_with", format!("ptr {a}, ptr {b}, ptr {same}"))
            }
            Kind::Dict(..) => self.call("i1", "zeno_dict_eq", format!("ptr {a}, ptr {b}")),
            Kind::Error => self.call("i1", "zeno_error_same", format!("ptr {a}, ptr {b}")),
            Kind::Optional(inner) => {
                let same = self.optional_same(inner, span)?;
                let ty = ir_type(kind, span)?;
                self.set(format!("call i1 {same}({ty} {a}, {ty} {b})"))
            }
            Kind::None => "true".to_string(),
            // Two function values are equal only if they are the same one.
            Kind::Function(_) => self.set(format!("icmp eq ptr {a}, {b}")),
            Kind::Other(_) | Kind::Owned(_) | Kind::CPointer(_) | Kind::CLibrary(_) | Kind::Several(_) => {
                return Err(cannot("comparing these", span))
            }
        })
    }

    /// The function that shows a `T or none` as text: the value inside,
    /// or the word `none`. One is written for each kind, the first time
    /// it is needed. `quoted` is for a str inside a list or an object.
    fn optional_text(&mut self, inner: &Kind, quoted: bool, span: Span) -> Result<String, Error> {
        let quoted = quoted && *inner == Kind::Str;
        let kind = Kind::Optional(Box::new(inner.clone()));
        let name = named('@', if quoted { "zeno.repr." } else { "zeno.text." }, &key(&kind));
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let ty = ir_type(&kind, span)?;
        self.side(format!("define internal ptr {name}({ty} %x)"), |emitter| {
            let x = Val::new("%x", kind.clone());
            let there = emitter.is_there(&x, span)?;
            emitter.jump_if(&there, "some", "none");
            emitter.place("some");
            let value = emitter.inside(&x, span)?;
            let shown = if quoted { emitter.repr(&value, span)? } else { emitter.text(&value, span)? };
            emitter.end(format!("ret ptr {shown}"));
            emitter.place("none");
            let none = emitter.string("none");
            emitter.end(format!("ret ptr {none}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The same, for comparing two of them: equal when both are `none`,
    /// or when neither is and the values inside are equal.
    fn optional_same(&mut self, inner: &Kind, span: Span) -> Result<String, Error> {
        let kind = Kind::Optional(Box::new(inner.clone()));
        let name = named('@', "zeno.same.", &key(&kind));
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        let ty = ir_type(&kind, span)?;
        self.side(format!("define internal i1 {name}({ty} %a, {ty} %b)"), |emitter| {
            let (a, b) = (Val::new("%a", kind.clone()), Val::new("%b", kind.clone()));
            let a_there = emitter.is_there(&a, span)?;
            let b_there = emitter.is_there(&b, span)?;
            let both = emitter.set(format!("and i1 {a_there}, {b_there}"));
            emitter.jump_if(&both, "both", "missing");
            emitter.place("both");
            let (a, b) = (emitter.inside(&a, span)?, emitter.inside(&b, span)?);
            let same = emitter.equal(&a.kind, &a.code, &b.code, span)?;
            emitter.end(format!("ret i1 {same}"));
            // At least one is `none`: equal only if the other is too.
            emitter.place("missing");
            let alike = emitter.set(format!("icmp eq i1 {a_there}, {b_there}"));
            emitter.end(format!("ret i1 {alike}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The runtime's list functions do not know what a list holds. To
    /// show a list they are handed a small function that shows one item,
    /// given its slot. This writes that function for a kind, the first
    /// time it is needed, and returns its name.
    fn repr_adapter(&mut self, kind: &Kind, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.item.repr.", &key(kind));
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        self.side(format!("define internal ptr {name}(i64 %item)"), |emitter| {
            let item = emitter.from_slot(kind, "%item", span)?;
            let shown = emitter.repr(&item, span)?;
            emitter.end(format!("ret ptr {shown}"));
            Ok(())
        })?;
        Ok(name)
    }

    /// The same, for comparing two items. C calls this one and reads a
    /// whole byte for the answer, which is what `zeroext` arranges.
    fn eq_adapter(&mut self, kind: &Kind, span: Span) -> Result<String, Error> {
        let name = named('@', "zeno.item.eq.", &key(kind));
        if !self.written.insert(name.clone()) {
            return Ok(name);
        }
        self.side(format!("define internal zeroext i1 {name}(i64 %a, i64 %b)"), |emitter| {
            let (a, b) = (emitter.from_slot(kind, "%a", span)?, emitter.from_slot(kind, "%b", span)?);
            let same = emitter.equal(kind, &a.code, &b.code, span)?;
            emitter.end(format!("ret i1 {same}"));
            Ok(())
        })?;
        Ok(name)
    }

    // ------------------------------------------------------------ f-strings

    /// `f"{name} is {age:>3}"`: each piece as text, then all of them joined.
    fn format_string(&mut self, parts: &[FormatPart], span: Span) -> Result<Val, Error> {
        // The values are worked out left to right, as they are written.
        // A number, a bool or a piece of text shows the same whenever it
        // is shown, so turning it into text waits until every value is
        // known. One of the later values may be a call, and text that is
        // built before the call takes room all the while it runs: with no
        // collector, room in the text room. (A list or an object is shown
        // at once: a later value may be a call that changes it.)
        enum Piece<'a> {
            Text(String),
            Waiting(Val, &'a Option<String>),
        }
        let mut waiting = Vec::new();
        for part in parts {
            waiting.push(match part {
                FormatPart::Text(literal) => Piece::Text(self.string(literal)),
                FormatPart::Value { value, format } => {
                    let val = self.expr(value)?;
                    match val.kind {
                        Kind::Int | Kind::Float | Kind::Bool | Kind::Pin | Kind::InputPin | Kind::Serial | Kind::AnalogPin | Kind::PwmPin | Kind::Ticker | Kind::Interrupt | Kind::Quantity(_) | Kind::Str => {
                            Piece::Waiting(val, format)
                        }
                        _ => Piece::Text(self.shown(&val, format, span)?),
                    }
                }
            });
        }
        let mut pieces = Vec::new();
        for piece in waiting {
            pieces.push(match piece {
                Piece::Text(text) => text,
                Piece::Waiting(val, format) => self.shown(&val, format, span)?,
            });
        }
        Ok(Val::new(
            match pieces.as_slice() {
                [] => self.string(""),
                [only] => only.clone(),
                _ => self.join(&pieces),
            },
            Kind::Str,
        ))
    }

    /// One value of an f-string as text, with its format if it has one.
    fn shown(&mut self, val: &Val, format: &Option<String>, span: Span) -> Result<String, Error> {
        match format {
            Some(spec) => self.formatted(val, spec, span),
            None => self.text(val, span),
        }
    }

    /// A value with a format after the colon, as in `{x:.2f}` or `{n:08b}`.
    /// The format is read now, by the same function the C backend uses.
    fn formatted(&mut self, val: &Val, spec: &str, span: Span) -> Result<String, Error> {
        let bad = || Error::new(format!("`{spec}` is not a format Zeno understands"), span);
        let emit_c::Format { align, zero, width, precision, kind } = emit_c::read_format(spec).ok_or_else(bad)?;

        // `{gain:.3f}`: the decimals of the float an f32 is exactly.
        if val.kind == Kind::F32 {
            return self.formatted(&Val::new(val.code.clone(), Kind::Float), spec, span);
        }
        // A quantity is a number with its unit, and lines up as numbers do.
        let is_number = matches!(val.kind, Kind::Int | Kind::Float | Kind::U64 | Kind::Quantity(_));
        let body = match (kind, precision, &val.kind) {
            // `{angle:.1f}` is `30.5 deg`: the decimals are the number's,
            // and the unit follows it as it always does.
            (Some('f'), _, Kind::Quantity(unit)) | (None, Some(_), Kind::Quantity(unit)) => {
                let unit = self.string(unit);
                let args = format!("double {}, i32 {}, ptr {unit}", val.code, precision.unwrap_or(6));
                self.call("ptr", "zeno_fixed_quantity", args)
            }
            (Some('f'), _, _) | (None, Some(_), Kind::Float) if is_number => {
                let x = self.double(val);
                self.call("ptr", "zeno_fixed", format!("double {x}, i32 {}", precision.unwrap_or(6)))
            }
            (None, None, _) | (Some('d'), None, Kind::Int | Kind::U64) => self.text(val, span)?,
            (Some(kind @ ('x' | 'X' | 'b')), None, Kind::U64) => {
                self.call("ptr", "zeno_radix_u64", format!("i64 {}, i8 {}", val.code, kind as u32))
            }
            // A character is handed over as its number: `x` is 120.
            (Some(kind @ ('x' | 'X' | 'b')), None, Kind::Int) => {
                self.call("ptr", "zeno_radix", format!("i64 {}, i8 {}", val.code, kind as u32))
            }
            _ => return Err(bad()),
        };
        if width == 0 {
            return Ok(body);
        }
        // Numbers line up on the right by default, everything else on the left.
        let align = align.unwrap_or(if is_number { '>' } else { '<' });
        let args = format!("ptr {body}, i32 {width}, i8 {}, i1 {}", align as u32, zero && is_number);
        Ok(self.call("ptr", "zeno_pad", args))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    pub(crate) fn compile(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_with_types(&program, true);
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        emit(&program, &types, "test.zeno", Build::Ordinary, false).map_err(|error| error.message)
    }

    /// The same, as `zeno build --llvm --firmware` compiles it.
    fn compile_firmware(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, Build::Firmware.rules());
        // A firmware build's check refuses a few programs that `zeno run`
        // takes (5.19m), and says so, as `zeno build --firmware` does.
        if let Some(error) = errors.first() {
            return Err(error.message.clone());
        }
        emit(&program, &types, "test.zeno", Build::Firmware, false).map_err(|error| error.message)
    }

    #[test]
    fn starting_another_program_is_left_to_zeno_run() {
        let message = compile("import program\nprint(program.run(\"ls\", [\"-l\"]))").err().unwrap_or_else(|| "it compiled".to_string());
        assert_eq!(message, "`zeno build --llvm` cannot compile `program.run` yet. `zeno run` can still run this program");
    }

    /// Build the IR and the runtime with clang, run the program, and
    /// return what it printed, what it wrote as errors, and whether it
    /// finished cleanly. `None` if there is no clang here to try it with.
    fn build_and_run(name: &str, ir: &str, firmware: bool) -> Option<(String, String, bool)> {
        let build = if firmware { Build::Firmware } else { Build::Ordinary };
        // ZENO_COLLECT_ALWAYS runs the garbage collector before every
        // single allocation, which is the setting most likely to show a
        // value the collector cannot find.
        build_and_run_as(name, ir, build, &["-DZENO_COLLECT_ALWAYS"])
    }

    /// The same, for any of the three kinds of build, with these flags
    /// for clang beside the usual ones.
    fn build_and_run_as(name: &str, ir: &str, build: Build, flags: &[&str]) -> Option<(String, String, bool)> {
        let ran = built_output(name, ir, build, flags)?;
        Some((
            String::from_utf8_lossy(&ran.stdout).into_owned(),
            String::from_utf8_lossy(&ran.stderr).into_owned(),
            ran.status.success(),
        ))
    }

    /// The same, with everything the run gave: the number it ended with
    /// among the rest.
    pub(crate) fn built_output(name: &str, ir: &str, build: Build, flags: &[&str]) -> Option<std::process::Output> {
        built_beside(name, ir, build, &runtime(build), flags)
    }

    /// The same, with this runtime in the file beside the IR.
    pub(crate) fn built_beside(name: &str, ir: &str, build: Build, runtime: &str, flags: &[&str]) -> Option<std::process::Output> {
        let dir = std::env::temp_dir().join(format!("zeno-test-{}-llvm-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary folder");
        let (ir_file, runtime_file, binary) = (dir.join("test.ll"), dir.join("test_runtime.c"), dir.join(emit_c::program_file("test")));
        std::fs::write(&ir_file, ir).expect("the IR file is written");
        let runtime = emit_c::with_board_parts(&emit_c::with_pieces(runtime, &emit_c::pieces_in_ir(ir)), &emit_c::board_parts_in_ir(ir));
        std::fs::write(&runtime_file, runtime).expect("the runtime is written");
        let built = Command::new("clang")
            .args(["-O2", "-Wno-override-module"])
            .args(flags)
            .arg("-o")
            .arg(&binary)
            .arg(&ir_file)
            .arg(&runtime_file)
            .args(emit_c::LIBRARIES)
            .output();
        let Ok(built) = built else {
            eprintln!("no clang found; skipping the compiled half of this test");
            return None;
        };
        let complaints = String::from_utf8_lossy(&built.stderr);
        let complaints: String = complaints.chars().take(1500).collect();
        assert!(built.status.success(), "the IR does not compile:\n{complaints}\n{ir}");
        // Set `ZENO_VALGRIND` when running the tests, and each program
        // built with no collector is run under valgrind, which stops a
        // program that frees a block twice, uses one it has freed, or
        // ends with one it never freed. (Not the other builds: a
        // collector reads the whole stack, which valgrind objects to.)
        let ran = match build == Build::NoCollector && std::env::var_os("ZENO_VALGRIND").is_some() {
            true => crate::value::testing::run(
                Command::new("valgrind").args(["-q", "--error-exitcode=99", "--leak-check=full"]).arg(&binary),
            ),
            false => crate::value::testing::run(&mut Command::new(&binary)),
        }
        .expect("the program runs");
        let _ = std::fs::remove_dir_all(&dir);
        Some(ran)
    }

    #[test]
    fn with_release_whole_numbers_wrap_round() {
        let source = "var n = 9223372036854775807\nn += 1\nprint(n)\nprint(n - 1)\nprint(-n)\nlet k = 4611686018427387904\nprint(k * 4)";
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_with_types(&program, true);
        let options = emit_c::Options { release: true, grows: false, fast: false, ..Default::default() };
        let ir = emit_with(&program, &types, "test.zeno", Build::Ordinary, false, options).expect("it builds");
        assert!(!ir.contains("with.overflow"), "{ir}");
        let Some((printed, errors, finished)) = build_and_run_as("release", &ir, Build::Ordinary, &[]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, "-9223372036854775808\n9223372036854775807\n-9223372036854775808\n0\n");
    }

    #[test]
    fn with_fast_an_item_and_a_float_division_are_plain_instructions() {
        let source = "import math\nlet xs = [1.5, 2.5, 4.0]\nlet i = 2\nlet z = 0.0\nprint(xs[i] / 2.0, xs[0] / z, math.sqrt(xs[2]), math.sqrt(-1.0))";
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_with_types(&program, true);
        let options = emit_c::Options { release: true, grows: false, fast: true, ..Default::default() };
        let ir = emit_with(&program, &types, "test.zeno", Build::Ordinary, false, options).expect("it builds");
        assert!(!ir.contains("zeno_list_at") && !ir.contains("@zeno.div") && !ir.contains("@zeno.sqrt"), "{ir}");
        assert!(ir.contains("fdiv double") && ir.contains("@llvm.sqrt.f64("), "{ir}");
        let flags = ["-DZENO_UNCHECKED", "-O3"];
        let Some((printed, errors, finished)) = build_and_run_as("fast", &ir, Build::Ordinary, &flags) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, "2.0 inf 2.0 NaN\n");
    }

    #[test]
    fn with_fast_whole_number_sums_say_they_do_not_overflow() {
        // `nsw` is what clang writes for a C program's signed `+`
        // (decision 23.6). `--release` leaves it out: there a sum wraps.
        let source = "function sum(n: int) -> int {\n    var total = 0\n    var i = 0\n    while i < n {\n        total += (i * i) % 7 - i\n        i += 1\n    }\n    return -total\n}\nprint(sum(1000), sum(0), 3 - 5 * 4)";
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_with_types(&program, true);
        let fast = emit_c::Options { release: true, grows: false, fast: true, ..Default::default() };
        let ir = emit_with(&program, &types, "test.zeno", Build::Ordinary, false, fast).expect("it builds");
        assert!(ir.contains("add nsw i64") && ir.contains("mul nsw i64") && ir.contains("sub nsw i64"), "{ir}");
        assert!(!ir.contains("with.overflow"), "{ir}");
        let release = emit_c::Options { release: true, grows: false, fast: false, ..Default::default() };
        let wraps = emit_with(&program, &types, "test.zeno", Build::Ordinary, false, release).expect("it builds");
        assert!(!wraps.contains(" nsw "), "{wraps}");
        let Some((printed, errors, finished)) = build_and_run_as("fast-sums", &ir, Build::Ordinary, &["-DZENO_UNCHECKED", "-O3"]) else { return };
        assert!(finished, "{errors}");
        assert_eq!(printed, "497499 0 -17\n");
    }

    #[test]
    fn dividing_by_a_power_of_two_is_one_instruction() {
        let source = "for x in [-17, -8, -1, 0, 7, 8, 1000] {\n    print(x // 8, x % 8, x // 1, x % 1, x // 3, x % 3)\n}";
        let program = crate::modules::testing::built(source);
        let (_, types) = crate::checker::check_with_types(&program, true);
        let ir = emit(&program, &types, "test.zeno", Build::Ordinary, false).expect("it builds");
        assert!(ir.contains("ashr i64") && ir.contains(", 7\n"), "{ir}");
        same_as_interpreter("power-of-two", source);
    }

    /// The heart of these tests, as for the C backend: the compiled
    /// program must print exactly what the interpreter and the VM print,
    /// and stop with the same error at the same place.
    pub(crate) fn same_as_interpreter(name: &str, source: &str) {
        let program = crate::modules::testing::program(source);
        let (interpreted, result) = crate::vm::run_both(&program, source);

        // Both kinds of build must agree with the interpreter: the
        // ordinary one, where errors jump, and `--firmware`, where they
        // travel as values.
        for firmware in [false, true] {
            let ir = if firmware {
                match compile_firmware(source) {
                    Ok(ir) => ir,
                    // A firmware build refuses a few things on purpose,
                    // and says so. Any other refusal is a failure.
                    Err(message) => {
                        assert!(message.contains("--firmware"), "the firmware build failed: {message}\n{source}");
                        continue;
                    }
                }
            } else {
                compile(source).expect("the test program compiles")
            };
            let name = if firmware { format!("{name}-firmware") } else { name.to_string() };
            let Some((compiled, errors, finished)) = build_and_run(&name, &ir, firmware) else {
                return;
            };
            assert_eq!(compiled, interpreted, "\nfirmware: {firmware}\n{source}");
            match &result {
                Ok(()) => assert!(finished, "the compiled program failed (firmware: {firmware}): {errors}"),
                Err(error) => {
                    assert!(!finished, "the compiled program should have failed");
                    let place = crate::modules::testing::place(&program, error.span);
                    let expected = format!("error: {}\n --> {place}\n", error.message);
                    assert_eq!(errors, expected, "\nfirmware: {firmware}\n{source}");
                }
            }
        }
    }

    #[test]
    fn numbers_match() {
        same_as_interpreter(
            "numbers",
            "print(7 + 2, 7 - 9, 7 * 6, 7 / 2, 7 // 2, -7 // 2, 7 % 3, -7 % 3, 7 % -3, 2 ** 10, 2 ** 0)\n\
             print(1.5 + 2, 3 - 0.5, 2 * 2.5, 7.0 / 2, 7.5 // 2, 7.5 % 2, 2 ** 0.5, 2.0 ** 3)\n\
             print(-(3 + 4), -2.5, - -3, 1e21, 1e-7, 0.1 + 0.2, 100.0, 3.0 * 1e10)\n\
             print(9223372036854775807, -9223372036854775807 - 1)\n\
             print(abs(-5), abs(-2.5), min(3, 1, 2), max(3, 1, 2), min(2.5, 1.5), max(2.5, 1.5))\n\
             print(int(3.9), int(-3.9), int(\"42\"), float(3), float(\"2.5\"), int(7), int(1e30))\n",
        );
    }

    #[test]
    fn division_written_in_ir_matches() {
        // `//`, `%` and `/` are not the runtime's here (see `WRITTEN`),
        // so every mix of signs is tried, and the ends of what an int holds.
        same_as_interpreter(
            "division",
            "let small = -9223372036854775807 - 1\n\
             let big = 9223372036854775807\n\
             var minus = 0\n\
             minus -= 1\n\
             print(big // minus, small // 2, small // big, 0 // 5)\n\
             print(small % minus, big % minus, small % 7, small % -7, big % small, 0 % 5)\n\
             print(7 / 2, -7 / 2, 1 / 3, 0 / 5, 7.5 / -2.5)\n\
             for a in range(-7, 8) {\n\
             \x20   for b in range(-3, 4) {\n\
             \x20       if b != 0 { print(a, b, a // b, a % b) }\n\
             \x20   }\n\
             }\n",
        );
        same_as_interpreter(
            "division-overflow",
            "let small = -9223372036854775807 - 1\nvar minus = 0\nminus -= 1\nprint(small // minus)\n",
        );
    }

    #[test]
    fn comparisons_and_logic_match() {
        same_as_interpreter(
            "logic",
            "print(1 < 2, 2 <= 2, 3 > 4, 4 >= 5, 1 == 1, 1 != 1, 1 == 1.0, 1 < 1.5, 2.5 >= 2)\n\
             print(1 < 2 < 3, 1 < 2 < 2, 3 > 2 > 1 > 0, 1 < 3 > 2, 5 < 4 < 3)\n\
             print(\"apple\" < \"banana\", \"a\" == \"a\", \"a\" != \"b\", \"b\" >= \"a\", true == true, true != false)\n\
             print(true and false, true or false, not true, not (1 > 2), 1 < 2 and 2 < 3 or false)\n\
             function loud(name: str, answer: bool) -> bool {\n\
             \x20   print(f\"asked {name}\")\n\
             \x20   return answer\n\
             }\n\
             function number(name: str, n: int) -> int {\n\
             \x20   print(f\"asked {name}\")\n\
             \x20   return n\n\
             }\n\
             print(loud(\"a\", false) and loud(\"b\", true))\n\
             print(loud(\"c\", true) or loud(\"d\", true))\n\
             print(loud(\"e\", true) and loud(\"f\", false) or loud(\"g\", true))\n\
             print(number(\"h\", 3) < number(\"i\", 2) < number(\"j\", 1))\n\
             print(number(\"k\", 1) < number(\"l\", 2) < number(\"m\", 3))\n",
        );
    }

    #[test]
    fn text_matches() {
        same_as_interpreter(
            "text",
            "let name = \"Zeno\"\n\
             var greeting = \"hello, \" + name\n\
             greeting += \"!\"\n\
             print(greeting, len(greeting), greeting[0], greeting[7], str(12) + str(1.5) + str(true))\n\
             print(name.upper(), name.lower(), \"  padded  \".strip(), name.replace(\"Z\", \"X\"))\n\
             print(name.starts_with(\"Ze\"), name.ends_with(\"x\"))\n\
             print(f\"{name}: {3.14159:.2f} | {42:>6} | {42:<6}| {255:x} {255:X} {5:b} | {7:03}\")\n\
             print(f\"{name:>8} | {name:^8} | {2.5:8.3f} | {10:d} | {true} | {1 + 2}\")\n\
             print(f\"no values\", f\"{1}{2}{3}\", f\"{name}\", \"\")\n\
             print(\"quote \\\" backslash \\\\ tab\\tend\", \"na\u{ef}ve caf\u{e9} \u{2615}\")\n\
             print()\n",
        );
    }

    #[test]
    fn control_flow_matches() {
        same_as_interpreter(
            "control",
            "function classify(n: int) -> str {\n\
             \x20   if n < 0 {\n\
             \x20       return \"negative\"\n\
             \x20   } else if n == 0 {\n\
             \x20       return \"zero\"\n\
             \x20   } else if n < 10 {\n\
             \x20       return \"small\"\n\
             \x20   } else {\n\
             \x20       return \"big\"\n\
             \x20   }\n\
             }\n\
             print(classify(-5), classify(0), classify(7), classify(70))\n\
             function collatz(start: int) -> int {\n\
             \x20   var n = start\n\
             \x20   var steps = 0\n\
             \x20   while n != 1 {\n\
             \x20       if n % 2 == 0 {\n\
             \x20           n = n // 2\n\
             \x20       } else {\n\
             \x20           n = 3 * n + 1\n\
             \x20       }\n\
             \x20       steps += 1\n\
             \x20   }\n\
             \x20   return steps\n\
             }\n\
             print(collatz(27))\n\
             var total = 0\n\
             for i in range(10) {\n\
             \x20   if i == 3 { continue }\n\
             \x20   if i == 8 { break }\n\
             \x20   total += i\n\
             }\n\
             print(total)\n\
             for i in range(2, 5) { print(i) }\n\
             for i in range(10, 0, -3) { print(i) }\n\
             for i in range(0, 10, 4) { print(i) }\n\
             for i in range(5, 5) { print(\"never\") }\n\
             var sum = 0\n\
             for a in range(4) {\n\
             \x20   for b in range(a) {\n\
             \x20       if b == 2 { break }\n\
             \x20       sum += a * b\n\
             \x20   }\n\
             }\n\
             var n = 0\n\
             while true {\n\
             \x20   n += 1\n\
             \x20   if n < 5 { continue }\n\
             \x20   break\n\
             }\n\
             print(sum, n)\n",
        );
    }

    #[test]
    fn functions_match() {
        same_as_interpreter(
            "functions",
            "print(fib(20), power(2), power(2, 5), power(exponent=3, base=4))\n\
             function fib(n: int) -> int {\n\
             \x20   if n < 2 { return n }\n\
             \x20   return fib(n - 1) + fib(n - 2)\n\
             }\n\
             function power(base: int, exponent: int = 2) -> int {\n\
             \x20   var result = 1\n\
             \x20   for i in range(exponent) { result *= base }\n\
             \x20   return result\n\
             }\n\
             function shout(what: str) {\n\
             \x20   print(what.upper() + \"!\")\n\
             \x20   return\n\
             \x20   print(\"never\")\n\
             }\n\
             shout(\"done\")\n\
             print(shout(\"again\"))\n\
             function shadow(n: int) -> int {\n\
             \x20   let x = n\n\
             \x20   var y = 1\n\
             \x20   if n > 0 {\n\
             \x20       let x = n * 2\n\
             \x20       var y = 10\n\
             \x20       y += x\n\
             \x20       return y\n\
             \x20   }\n\
             \x20   return x + y\n\
             }\n\
             print(shadow(4), shadow(0))\n\
             function average(a: float, b: float) -> float { return (a + b) / 2 }\n\
             print(average(1.0, 2.0), average(b=4.0, a=1.5))\n\
             function untyped(a, b) { return a * b + 1 }\n\
             print(untyped(3, 4))\n",
        );
    }

    #[test]
    fn variables_at_the_top_are_shared_with_functions() {
        same_as_interpreter(
            "globals",
            "var counter = 0\n\
             let step = 5\n\
             var log = \"\"\n\
             function bump() -> int {\n\
             \x20   counter += step\n\
             \x20   log += f\"{counter} \"\n\
             \x20   return counter\n\
             }\n\
             bump()\n\
             bump()\n\
             print(counter, bump(), log)\n\
             if counter > 10 {\n\
             \x20   let inside = counter * 2\n\
             \x20   var changing = inside\n\
             \x20   changing += 1\n\
             \x20   print(inside, changing)\n\
             }\n",
        );
        // A function means the built-in `len` when the `len` of the
        // program is declared below it (decision 3.6d).
        same_as_interpreter("below", "function size(text: str) -> int { return len(text) }\nlet len = 5\nprint(size(\"abc\"), len)\n");
    }

    #[test]
    fn text_survives_the_collector() {
        // Text that is held only by a value in the IR, or by a global,
        // while a great deal more is made and thrown away.
        same_as_interpreter(
            "collector",
            "var kept = \"start\"\n\
             function grow(times: int) -> str {\n\
             \x20   var text = \"\"\n\
             \x20   for i in range(times) {\n\
             \x20       let piece = f\"{i},\"\n\
             \x20       text += piece\n\
             \x20       if i % 50 == 0 { kept = text.upper() + \"!\" }\n\
             \x20   }\n\
             \x20   return text\n\
             }\n\
             let made = grow(120)\n\
             var again = \"\"\n\
             for i in range(300) {\n\
             \x20   again = str(i) + made[i % 10]\n\
             }\n\
             print(len(made), len(kept), made[0], kept[5], again)\n",
        );
    }

    #[test]
    fn errors_stop_the_program_the_same_way() {
        let programs = [
            "print(\"before\")\nlet big = 9223372036854775807\nprint(big + 1)\n",
            "let big = 9223372036854775807\nprint(big * 2)\n",
            "var low = -9223372036854775807\nlow -= 2\nprint(low)\n",
            "let low = -9223372036854775807 - 1\nprint(-low)\n",
            "let low = -9223372036854775807 - 1\nprint(abs(low))\n",
            "let zero = 0\nprint(1 // zero)\n",
            "let zero = 0\nprint(1 % zero)\n",
            "let zero = 0\nprint(1 / zero)\n",
            "let zero = 0.0\nprint(1.5 // zero)\n",
            "print(2 ** -1)\n",
            "print(int(\"abc\"))\n",
            "print(float(\"x1\"))\n",
            "let text = \"abc\"\nprint(text[5])\n",
            "for i in range(0, 10, 0) { print(i) }\n",
            "function deep(n: int) -> int {\n    if n == 0 { return 1 // n }\n    return deep(n - 1)\n}\nprint(\"start\")\nprint(deep(3))\n",
        ];
        for (index, program) in programs.iter().enumerate() {
            same_as_interpreter(&format!("error-{index}"), program);
        }
    }

    #[test]
    fn the_examples_it_can_compile_match() {
        same_as_interpreter("hello", include_str!("../examples/hello.zeno"));
        same_as_interpreter("primes", include_str!("../examples/primes.zeno"));
        same_as_interpreter("tour", include_str!("../examples/tour.zeno"));
        same_as_interpreter("styles", include_str!("../examples/styles.zeno"));
        same_as_interpreter("shapes", include_str!("../examples/shapes.zeno"));
        same_as_interpreter("sensor", include_str!("../examples/sensor.zeno"));
        same_as_interpreter("memory", include_str!("../examples/memory.zeno"));
        same_as_interpreter("buffer", include_str!("../examples/buffer.zeno"));
        same_as_interpreter("signal", include_str!("../examples/signal.zeno"));
        same_as_interpreter("packet", include_str!("../examples/packet.zeno"));
        same_as_interpreter("generics", include_str!("../examples/generics.zeno"));
        same_as_interpreter("labels", include_str!("../examples/labels.zeno"));
        same_as_interpreter("words", include_str!("../examples/words.zeno"));
        same_as_interpreter("scheduler", include_str!("../examples/scheduler.zeno"));
        same_as_interpreter("commands", include_str!("../examples/commands.zeno"));
        same_as_interpreter("handlers", include_str!("../examples/handlers.zeno"));
        same_as_interpreter("driver", include_str!("../examples/driver.zeno"));
        same_as_interpreter("ports", include_str!("../examples/ports.zeno"));
        same_as_interpreter("channels", include_str!("../examples/channels.zeno"));
    }

    #[test]
    fn classes_match() {
        same_as_interpreter(
            "classes",
            "class Point {\n    x: float = 0.0\n    y: float = 0.0\n    name: str = \"p\"\n\
             \x20   function moved(self, dx: float = 1.0, dy: float = 0.0) -> Point {\n\
             \x20       return Point(self.x + dx, self.y + dy, self.name)\n    }\n\
             \x20   function shout(self) { print(self.name.upper(), self.x, self.y) }\n}\n\
             class Empty { }\n\
             class Node {\n    value: int\n    next: Node or none\n}\n\
             let p = Point(y=2.0)\n\
             p.moved(dy=5.0).shout()\n\
             p.x = 7.5\np.x += 1\np.shout()\n\
             print(p, p == Point(8.5, 2.0), Point() == Point(), p == Point())\n\
             print(Empty(), Empty() == Empty(), [Empty()])\n\
             var chain: Node or none = none\n\
             for i in range(4) { chain = Node(i, chain) }\n\
             print(chain, chain == Node(3, Node(2, Node(1, Node(0, none)))), chain == Node(3, none))\n",
        );
    }

    #[test]
    fn lists_match() {
        same_as_interpreter(
            "lists",
            "let grid: list[list[int]] = [[1, 2], [3, 4], []]\n\
             grid[2].append(9)\ngrid[0][1] = 20\ngrid[1][0] += 5\n\
             print(grid, len(grid), grid[1], grid == [[1, 20], [8, 4], [9]], [9] in grid, [7] not in grid)\n\
             print([n * n for n in range(6) if n % 2 == 1], [c.upper() for c in \"abc\"], [len(row) for row in grid])\n\
             print(sum([1, 2, 3]), sum([0.5, 0.25]), max([1.5, 0.5]), min([4, 2, 8]), [1] + [2, 3])\n\
             print(\"a b  c\".split(), \"a,b\".split(\",\"), \"b\" in \"abc\", \"z\" not in \"abc\")\n\
             var words: list[str] = []\n\
             for word in \"the quick brown\".split() { words.append(word.upper()) }\n\
             print(words, words.pop(), words, [true, false], [1.5, 2.0], [1 s, 2 s])\n\
             var seen = 0\n\
             for word in words {\n    words.append(\"again\")\n    seen += 1\n}\n\
             print(seen, len(words))\n\
             for letter in \"hey\" { print(letter) }\n\
             print(words[9])\n",
        );
    }

    #[test]
    fn enums_and_match_match() {
        same_as_interpreter(
            "enums",
            "enum Shape {\n    Circle(r: float)\n    Rect(w: float, h: float)\n    Dot\n    Label(text: str, of: Shape or none)\n}\n\
             function area(s: Shape) -> float {\n    match s {\n\
             \x20       case Circle(r) { return 3.0 * r * r }\n\
             \x20       case Rect(w, h) { return w * h }\n\
             \x20       case Dot { return 0.0 }\n\
             \x20       case Label(_, inner) {\n            if inner is not none { return area(inner) }\n            return -1.0\n        }\n\
             \x20   }\n}\n\
             let shapes = [Circle(1.0), Rect(2.0, 3.0), Dot, Label(\"hi\", Circle(2.0)), Label(\"no\", none)]\n\
             for s in shapes { print(s, area(s)) }\n\
             print(shapes, Dot == Dot, Circle(1.0) == Circle(1.0), Circle(1.0) == Circle(2.0), Circle(1.0) == Dot)\n\
             print(Rect(2.0, 3.0) in shapes, Shape.Dot, Shape.Circle(0.5), Label(\"a\", Dot) == Label(\"a\", Dot))\n\
             function describe(n: int) -> str {\n    match n {\n\
             \x20       case 0 { return \"zero\" }\n        case 1 { return \"one\" }\n        case other { return f\"many ({other})\" }\n\
             \x20   }\n}\n\
             print(describe(0), describe(1), describe(7))\n\
             match \"x\" + \"y\" {\n    case \"xy\" { print(\"joined\") }\n    case _ { print(\"no\") }\n}\n\
             match Rect(1.0, 2.0) {\n    case Rect(1.0, h) { print(\"one wide\", h) }\n    case _ { print(\"no\") }\n}\n",
        );
    }

    #[test]
    fn values_that_may_be_none_match() {
        same_as_interpreter(
            "none",
            "function find(items: list[str], want: str) -> int or none {\n\
             \x20   for i in range(len(items)) { if items[i] == want { return i } }\n}\n\
             let at = find([\"a\", \"b\"], \"b\")\n\
             print(at, find([\"a\"], \"z\"), at == 1, at is not none, at is none, find([], \"a\") is none)\n\
             if at is not none { print(at + 1) }\n\
             match at {\n    case none { print(\"nothing\") }\n    case 1 { print(\"one\") }\n    case n { print(\"other\", n) }\n}\n\
             let maybe: list[int or none] = [1, none, 3]\n\
             print(maybe, maybe[1], maybe[2], maybe == [1, none, 3], none in maybe, 2 in maybe, 3 in maybe)\n\
             var flags: list[bool or none] = [true, none, false]\n\
             flags.append(none)\nflags[0] = false\n\
             print(flags, flags.pop(), len(flags))\n\
             let halves: list[float or none] = [1.5, none]\n\
             print(halves, halves[0] == 1.5, halves[1] is none)\n\
             var name: str or none = none\n\
             print(name, name is none)\n\
             name = \"zeno\"\n\
             if name is not none { print(name.upper(), [name]) }\n\
             var level: float or none = 2.5\n\
             function raise_it() { level = 4.0 }\n\
             if level is not none {\n    print(level * 2)\n    raise_it()\n    print(level * 2)\n}\n",
        );
    }

    #[test]
    fn a_shared_variable_is_looked_at_again_after_a_check() {
        // `clear()` sets `n` back to none after the check. The compiled
        // program must stop, not carry on with a number that is not there.
        let source = "var n: int or none = 5\nfunction clear() { n = none }\nif n is not none {\n clear()\n print(n + 1)\n}";
        let ir = compile(source).expect("it compiles");
        assert!(ir.contains("call void @zeno_gone(i32 5, i32 8)"), "{ir}");
        let Some((printed, errors, finished)) = build_and_run("gone", &ir, false) else {
            return;
        };
        assert_eq!(printed, "");
        assert!(!finished);
        assert_eq!(
            errors,
            "error: this was checked with `is not none`, but it has been set to `none` since\n --> test.zeno:5:8\n"
        );
        // A variable nothing else can reach needs no second look.
        let ir = compile("function f(n: int or none) -> int {\n if n is not none { return n + 1 }\n return 0\n}\nprint(f(1))")
            .expect("it compiles");
        assert!(!ir.contains("zeno_gone"), "{ir}");
    }

    #[test]
    fn errors_are_raised_and_caught_the_same_way() {
        // `total` and `seen` change inside the `try` and are read after
        // an error has jumped out of it: the case `volatile` is for.
        same_as_interpreter(
            "errors",
            "function risky(n: int) -> int raises ValueError {\n\
             \x20   if n > 2 { raise ValueError(f\"too big: {n}\") }\n    return n * 10\n}\n\
             function count_up() -> int {\n    var total = 0\n    var seen = 0\n\
             \x20   for i in range(6) {\n        try {\n            seen = seen + 1\n            total = total + risky(i)\n            seen = seen + 100\n\
             \x20       } except ValueError as err {\n            total = total + 1\n            print(\"caught\", err.message, err.kind, seen, total)\n        }\n    }\n\
             \x20   return total + seen\n}\n\
             print(count_up())\n\
             function early(n: int) -> int {\n    var steps = 0\n    while true {\n        steps = steps + 1\n\
             \x20       try {\n            if steps == n { return steps * 2 }\n            if steps % 2 == 0 { continue }\n            if steps > 50 { break }\n\
             \x20           let x = 10 // (steps - 3)\n            steps = steps + x - x\n\
             \x20       } except ZeroDivisionError {\n            print(\"zero at\", steps)\n        }\n    }\n    return -1\n}\n\
             print(early(7), early(100))\n\
             class Fault {\n    code: int\n    place: str\n}\n\
             function deep(n: int) raises Fault {\n    if n == 0 { raise Fault(42, \"deep\") }\n    deep(n - 1)\n}\n\
             try {\n    try { deep(5) } except ValueError { print(\"no\") }\n} except Fault as fault {\n    print(\"fault\", fault.code, fault.place, fault)\n}\n\
             try { deep(2) } except Error as error { print(\"any\", error, error.kind) }\n\
             function nested() -> str {\n    var log = \"\"\n    try {\n        log = log + \"a\"\n\
             \x20       try {\n            log = log + \"b\"\n            raise ValueError(\"inner\")\n\
             \x20       } except ValueError as error {\n            log = log + \"c\"\n            raise Error(\"outer \" + error.message)\n        }\n\
             \x20   } except Error as error {\n        log = log + \"d:\" + error.message\n    }\n    return log\n}\n\
             print(nested())\n\
             print(int(\"12x\"))\n",
        );
    }

    #[test]
    fn a_var_changed_in_a_try_is_kept_in_memory() {
        let ir = compile(
            "function f() -> int {\n    var n = 0\n    try { n = 1 } except Error { n = 2 }\n    return n\n}\nprint(f())\n",
        )
        .expect("it compiles");
        assert!(ir.contains("store volatile i64 1, ptr %var.n"), "{ir}");
        assert!(ir.contains(&format!("call i32 @_setjmp(ptr %try.1{SETJMP_TAKES}) returns_twice")), "{ir}");
        // A function with no `try` in it has no need of that.
        let ir = compile("function f() -> int {\n    var n = 0\n    n = 1\n    return n\n}\nprint(f())\n").expect("it compiles");
        assert!(!ir.contains("volatile"), "{ir}");
    }

    // ---------------------------------------------- Errors as values (firmware)

    /// `same_as_interpreter`, for a program that a firmware build must
    /// accept, so that the firmware half of the comparison really runs.
    pub(crate) fn firmware_as_interpreter(name: &str, source: &str) {
        if let Err(message) = compile_firmware(source) {
            panic!("the firmware build through LLVM refused this: {message}");
        }
        same_as_interpreter(name, source);
    }

    #[test]
    fn a_firmware_build_has_nothing_to_jump_with() {
        let source = include_str!("../examples/sensor.zeno");
        let ir = compile_firmware(source).expect("it compiles for firmware");
        for word in ["setjmp", "zeno_try", "zeno_raise(", "zeno_raise_again", "zeno_caught", "volatile"] {
            assert!(!ir.contains(word), "`{word}` is in the IR of a firmware build");
        }
        // A function that can raise says whether it worked, and gives its
        // answer through a pointer. `none` is an answer like any other.
        assert!(ir.contains("define internal zeroext i1 @z_parse_line(ptr %arg.line, ptr %result) {"), "{ir}");
        assert!(ir.contains("  store { i1, double } zeroinitializer, ptr %result\n  ret i1 true\n"), "{ir}");
        // `float(text)` can fail, so what it gives back is tested, and a
        // failure is passed straight on to the caller.
        assert!(
            ir.contains(
                "  %t4 = call zeroext i1 @zeno_parse_float(ptr %t1, i32 18, i32 17, ptr %answer.2)\n  \
                 br i1 %t4, label %worked3, label %failed3\n\nfailed3:\n  ret i1 false\n\nworked3:\n  \
                 %t5 = load double, ptr %answer.2\n"
            ),
            "{ir}"
        );
        // `raise` notes the error and gives back 0.
        assert!(ir.contains("  call void @zeno_note(ptr %t13, i32 20, i32 9)\n  ret i1 false\n"), "{ir}");
        assert!(ir.contains("declare zeroext i1 @zeno_parse_float(ptr, i32 signext, i32 signext, ptr)"), "{ir}");
        // A function that cannot raise is compiled exactly as before.
        let ordinary = compile(source).expect("it compiles");
        let in_range = |ir: &str| {
            let from = ir.find("define internal ptr @z_in_range(").expect("in_range is there");
            ir[from..].split("\n}\n").next().expect("its end").to_string()
        };
        assert_eq!(in_range(&ir), in_range(&ordinary));
    }

    #[test]
    fn the_ir_for_a_try_in_a_firmware_build() {
        let ir = compile_firmware(
            "function parse(text: str) -> int raises ValueError {\n    return int(text)\n}\n\
             function safe(text: str) -> int {\n    try {\n        return parse(text)\n    } except ValueError as e {\n        return -1\n    }\n}\n\
             print(safe(\"4\"), safe(\"x\"))\nprint(parse(\"7\"))\n",
        )
        .expect("it compiles for firmware");
        // Inside the `try`, a failed call goes to the `except` blocks...
        assert!(ir.contains("failed3:\n  br label %except1\n"), "{ir}");
        // ...where the first question is whether anything failed at all,
        // and the second is whether this `except` wants it.
        assert!(
            ir.contains(
                "except1:\n  %t3 = load ptr, ptr @zeno_raised\n  %t4 = icmp ne ptr %t3, null\n  \
                 br i1 %t4, label %caught1, label %end1\n\ncaught1:\n  store ptr null, ptr @zeno_raised\n  \
                 %t5 = call i1 @zeno_is(ptr %t3, ptr @.str."
            ),
            "{ir}"
        );
        // At the top of the file nobody is left to catch it.
        assert!(ir.contains("  call void @zeno_uncaught()\n  unreachable\n"), "{ir}");
    }

    #[test]
    fn what_a_firmware_build_refuses_says_so() {
        let refused = |source: &str| compile_firmware(source).err().unwrap_or_else(|| "it compiled".to_string());
        // The words are the C backend's: one message for one thing.
        let message = refused("let zero = 0\ntry { print(1 // zero) } except ZeroDivisionError { print(\"no\") }");
        assert!(message.starts_with("nothing in this `try` can be caught. In a firmware build"), "{message}");
        assert!(message.ends_with("Test for it before it happens, or build without `--firmware`"), "{message}");
        let parse = "function parse(text: str) -> int raises ValueError { return int(text) }\n";
        let message = refused(&format!("{parse}try {{ print(parse(\"1\")) }} except IndexError {{ print(\"no\") }}"));
        assert!(message.starts_with("nothing in this `try` can reach `except IndexError`."), "{message}");
        // A function that can raise, used as a value, builds since a
        // function type can say `raises` (decision 56.5).
        assert!(compile_firmware(&format!("{parse}print([\"1\"].map(parse))")).is_ok());
        assert!(compile_firmware("let f = function(text: str) -> int raises ValueError { return int(text) }\nprint(1)").is_ok());
        assert!(compile(&format!("{parse}print([\"1\"].map(parse))")).is_ok());
    }

    #[test]
    fn a_fault_stops_a_firmware_build_where_an_ordinary_one_catches_it() {
        // The one place the two kinds of build differ, on purpose, and
        // they differ here exactly as the two C builds do.
        let source = "function parse(text: str) -> int raises ValueError { return int(text) }\nlet zero = 0\ntry {\n print(parse(\"1\"))\n print(1 // zero)\n} except {\n print(\"caught\")\n}\ntry { print(parse(\"x\")) } except { print(\"caught too\") }";
        let Some((printed, _, finished)) = build_and_run("fault", &compile(source).unwrap(), false) else {
            return;
        };
        assert_eq!(printed, "1\ncaught\ncaught too\n");
        assert!(finished);
        let (printed, errors, finished) =
            build_and_run("fault-firmware", &compile_firmware(source).unwrap(), true).unwrap();
        assert_eq!(printed, "1\n");
        assert!(!finished);
        assert_eq!(errors, "error: uncaught ZeroDivisionError: division by zero\n --> test.zeno:5:8\n");
    }

    // ------------------------------------------------------- Owned values

    /// A small program with an owned class in it, an owned list, a value
    /// given away and a function that takes one over.
    const FRAME: &str = "owned class Frame {
    id: int
    bytes: owned list[int]
}

function make(id: int) -> Frame {
    let bytes: owned list[int] = [id, id + 1]
    return Frame(id, bytes)
}

function size(frame: take Frame) -> int {
    return len(frame.bytes)
}

let frame = make(7)
print(size(frame))
";

    #[test]
    fn the_ir_for_an_owned_value() {
        let ir = compile(FRAME).expect("it compiles");
        // An object of an owned class frees what its fields own, then
        // itself. Null means it was given away.
        let drop = "define internal void @zeno.drop.Frame(ptr %self) {
entry:
  %t1 = icmp eq ptr %self, null
  br i1 %t1, label %gone1, label %there1

gone1:
  ret void

there1:
  %t2 = getelementptr %z_Frame, ptr %self, i32 0, i32 1
  %t3 = load ptr, ptr %t2
  call void @zeno_list_drop(ptr %t3)
  call void @zeno_free(ptr %self)
  ret void
}";
        assert!(ir.contains(drop), "{ir}");
        // An owner has a slot, even one declared with `let`. Giving the
        // value away leaves null behind, and the end of the block frees
        // what is still there, which by then is nothing.
        let gives = "  store ptr %t4, ptr %var.bytes
  %t5 = load ptr, ptr %var.bytes
  store ptr null, ptr %var.bytes
  %t6 = call ptr @zeno.new.Frame(i64 %arg.id, ptr %t5)
  %t7 = load ptr, ptr %var.bytes
  call void @zeno_list_drop(ptr %t7)
  ret ptr %t6";
        assert!(ir.contains(gives), "{ir}");
        // A function that takes a value over frees it, after the answer
        // has been worked out.
        let takes = "define internal i64 @z_size(ptr %arg.frame) {
entry:
  %var.frame = alloca ptr
  store ptr %arg.frame, ptr %var.frame
  %t1 = load ptr, ptr %var.frame
  %t2 = getelementptr %z_Frame, ptr %t1, i32 0, i32 1
  %t3 = load ptr, ptr %t2
  %t4 = load i64, ptr %t3
  %t5 = load ptr, ptr %var.frame
  call void @zeno.drop.Frame(ptr %t5)
  ret i64 %t4
}";
        assert!(ir.contains(takes), "{ir}");
    }

    /// What a program built with `--memory` says as it ends: the numbers
    /// of its report, in order.
    fn memory_of(name: &str, source: &str) -> Option<(String, Vec<usize>)> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_with_types(&program, true);
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        let ir = emit(&program, &types, "test.zeno", Build::Ordinary, true).expect("it compiles");
        let flags = ["-DZENO_MEMORY_REPORT", "-DZENO_FIRST_COLLECTION=16384"];
        let (printed, errors, finished) = build_and_run_as(name, &ir, Build::Ordinary, &flags)?;
        assert!(finished, "the compiled program failed: {errors}");
        let report = errors.lines().find(|line| line.starts_with("memory:")).expect("a memory report");
        let numbers = report
            .split(|c: char| !c.is_ascii_digit())
            .filter(|word| !word.is_empty())
            .map(|word| word.parse().expect("a number"))
            .collect();
        Some((printed, numbers))
    }

    #[test]
    fn owned_values_are_freed_without_the_collector() {
        // Two thousand lists, one after another. Shared, they are left
        // for the collector, which has to run to get the memory back.
        // Owned, each is freed where its block ends, and the collector
        // never has anything to do.
        let work = |list: &str| {
            format!(
                "function work(rounds: int) -> int {{
    var total = 0
    for round in range(rounds) {{
        let scratch: {list} = [round, round + 1, round + 2]
        total += len(scratch)
    }}
    return total
}}
print(work(2000))
"
            )
        };
        let Some((shared_printed, shared)) = memory_of("shared-work", &work("list[int]")) else { return };
        let Some((owned_printed, owned)) = memory_of("owned-work", &work("owned list[int]")) else { return };
        assert_eq!(shared_printed, "6000\n");
        assert_eq!(owned_printed, "6000\n");
        // blocks made, bytes, most in use at once, collections, and for
        // the owned ones, how many were freed without the collector: a
        // list is two blocks.
        assert!(shared[3] > 0 && shared.len() == 4, "{shared:?}");
        assert_eq!((owned[3], owned[4]), (0, 4000), "{owned:?}");
        assert!(owned[2] < shared[2], "owned: {owned:?}, shared: {shared:?}");
    }

    // ---------------------------------------------- With no collector to help

    /// The same, as `zeno build --llvm --no-collector --memory` compiles it.
    pub(crate) fn compile_no_collector(source: &str) -> Result<String, String> {
        let program = crate::modules::testing::built(source);
        let (errors, types) = crate::checker::check_by(&program, Build::NoCollector.rules());
        if let Some(error) = errors.first() {
            return Err(error.message.clone());
        }
        emit(&program, &types, "test.zeno", Build::NoCollector, true).map_err(|error| error.message)
    }

    /// A program that the C build with no collector accepted and ran.
    /// `printed` is what it printed and `report` what it wrote as it
    /// ended: how many blocks it made, how many bytes, how much text was
    /// in the text room at once, and that no block was left.
    ///
    /// Built through LLVM the program must print the same and write the
    /// same report, line for line. So the two backends take and give back
    /// memory at the same places, and neither leaves a block behind.
    pub(crate) fn no_collector_as_c(name: &str, source: &str, printed: &str, report: &str) {
        let ir = match compile_no_collector(source) {
            Ok(ir) => ir,
            Err(message) => panic!("the build with no collector through LLVM refused this: {message}"),
        };
        // The point of the build: none of this is in the IR.
        for gone in ["zeno_collect", "zeno_roots", "zeno_stack_base", "zeno_mark_words", "_setjmp", "volatile"] {
            assert!(!ir.contains(gone), "`{gone}` is still in a build with no collector");
        }
        assert_eq!(asks_the_heap(&ir), None, "the IR takes memory from the heap");
        let name = format!("{name}-llvm");
        let Some((compiled, errors, finished)) = build_and_run_as(&name, &ir, Build::NoCollector, &["-DZENO_MEMORY_REPORT"])
        else {
            return;
        };
        assert!(finished, "the compiled program failed: {errors}");
        assert_eq!(compiled, printed, "\n{source}");
        assert_eq!(errors, report, "the two backends do not use memory alike\n{source}");
    }

    /// Does this C, or this IR, take memory from the C library's heap?
    /// The answer is the first of its four names that is found. A build
    /// with no collector must not: its memory is the pool in runtime.c
    /// (docs/pool.md). The names are looked for everywhere, comments
    /// included, because a reviewer's search would look there too. The
    /// runtime's own `zeno_free` is not the library's `free`.
    pub(crate) fn asks_the_heap(code: &str) -> Option<&'static str> {
        let free = code.match_indices("free(").any(|(at, _)| !code[..at].ends_with("zeno_"));
        ["malloc", "calloc", "realloc"].into_iter().find(|name| code.contains(name)).or(free.then_some("free"))
    }

    /// A program that the C build with no collector refused, with these
    /// words. The LLVM build must refuse it in the same words.
    pub(crate) fn refuses_as_c(source: &str, message: &str) {
        let ours = compile_no_collector(source).err().unwrap_or_else(|| "it compiled".to_string());
        assert_eq!(ours, message, "\n{source}");
    }

    #[test]
    fn the_ir_for_a_build_with_no_collector() {
        let ir = compile_no_collector(FRAME).expect("it compiles");
        // No list of roots, and no note of where the stack begins.
        assert!(!ir.contains("zeno_roots") && !ir.contains("zeno_stack_base") && !ir.contains("%base"), "{ir}");
        // `print` builds text, to show the number. The top of the text
        // room is noted before the statement and put back after it, and
        // then the end of the file frees what `frame` still holds.
        let top = "define internal void @zeno_program() noinline {
entry:
  %t1 = call ptr @z_make(i64 7)
  store ptr %t1, ptr @z_frame
  %mark.1 = load i64, ptr @zeno_text_top
  %t2 = load ptr, ptr @z_frame
  store ptr null, ptr @z_frame
  %t3 = call i64 @z_size(ptr %t2)
  %t4 = call ptr @zeno_str_int(i64 %t3)
  call void (i32, ...) @zeno_print(i32 1, ptr %t4)
  store i64 %mark.1, ptr @zeno_text_top
  %t5 = load ptr, ptr @z_frame
  call void @zeno.drop.Frame(ptr %t5)
  ret void
}";
        assert!(ir.contains(top), "{ir}");
        // A statement that builds no text has no such lines.
        let make = ir.split("define internal ptr @z_make(").nth(1).expect("make").split("\n}\n").next().expect("its body");
        assert!(!make.contains("zeno_text_top"), "{make}");
    }

    #[test]
    fn the_ir_for_what_a_statement_owns() {
        // docs/temporaries.md, in IR.
        let ir = compile_no_collector(
            "function longest(words: list[str]) -> int {
    var most = 0
    for word in words {
        if len(word) > most { most = len(word) }
    }
    return most
}

let line = \"set led on\"
print(longest(line.split()))
let ok = len(line) > 4 and len(line.split()) == 3
print(ok)
",
        )
        .unwrap();
        let program = ir.split("define internal void @zeno_program() noinline {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // The list of words is kept in a slot of the statement's own:
        // null where the statement begins, freed where it ends. The right
        // side of `and` is over before the statement is, so the list it
        // made is freed in its own block.
        assert_eq!(
            program,
            "entry:
  %own.1 = alloca ptr
  %own.2 = alloca ptr
  store ptr @.str.1, ptr @z_line
  %mark.1 = load i64, ptr @zeno_text_top
  store ptr null, ptr %own.1
  %t1 = load ptr, ptr @z_line
  %t2 = call ptr @zeno_split_blanks(ptr %t1)
  store ptr %t2, ptr %own.1
  %t3 = call i64 @z_longest(ptr %t2)
  %t4 = call ptr @zeno_str_int(i64 %t3)
  call void (i32, ...) @zeno_print(i32 1, ptr %t4)
  %t5 = load ptr, ptr %own.1
  call void @zeno_list_drop_text(ptr %t5)
  store i64 %mark.1, ptr @zeno_text_top
  %t6 = load ptr, ptr @z_line
  %t7 = call i64 @zeno_len(ptr %t6)
  %t8 = icmp sgt i64 %t7, 4
  br i1 %t8, label %and1, label %end1

and1:
  store ptr null, ptr %own.2
  %t9 = load ptr, ptr @z_line
  %t10 = call ptr @zeno_split_blanks(ptr %t9)
  store ptr %t10, ptr %own.2
  %t11 = load i64, ptr %t10
  %t12 = icmp eq i64 %t11, 3
  %t13 = load ptr, ptr %own.2
  call void @zeno_list_drop_text(ptr %t13)
  br label %end1

end1:
  %t14 = phi i1 [ false, %entry ], [ %t12, %and1 ]
  store i1 %t14, ptr @z_ok
  %mark.2 = load i64, ptr @zeno_text_top
  %t15 = load i1, ptr @z_ok
  %t16 = call ptr @zeno_str_bool(i1 %t15)
  call void (i32, ...) @zeno_print(i32 1, ptr %t16)
  store i64 %mark.2, ptr @zeno_text_top
  ret void"
        );
    }

    #[test]
    fn the_ir_for_a_list_that_owns_its_items() {
        // docs/one-owner.md, in IR.
        let ir = compile_no_collector(
            "class Point {\n    x: int\n}\n\
             let points = [Point(1), Point(2)]\n\
             points[0] = Point(3)\n\
             for p in points {\n    print(p.x, len(points))\n}\n\
             let grid = [[points.pop()]]\n",
        )
        .unwrap();
        // One function for each kind of list that frees its items: it
        // walks the list, frees each item, then frees the list alone.
        let drop = ir.split("define internal void @zeno_list_drop_1_Point(ptr %list) {\n").nth(1).expect("the function");
        let drop = drop.split("\n}\n").next().unwrap();
        assert!(drop.contains("  call void @zeno.drop.Point(ptr %t9)\n"), "{drop}");
        assert!(drop.ends_with("done2:\n  call void @zeno_list_drop(ptr %list)\n  ret void"), "{drop}");
        // A list of lists of Points frees each item with the one above.
        let outer = ir.split("define internal void @zeno_list_drop_2_Point(ptr %list) {\n").nth(1).expect("the function");
        assert!(outer.split("\n}\n").next().unwrap().contains("  call void @zeno_list_drop_1_Point(ptr %t9)\n"), "{ir}");

        let program = ir.split("define internal void @zeno_program() noinline {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // A new item frees the one whose place it takes.
        assert!(
            program.contains(
                "  %t9 = call ptr @zeno_list_at(ptr %t7, i64 0, i32 5, i32 1)
  %t10 = load ptr, ptr %t9
  call void @zeno.drop.Point(ptr %t10)
  store i64 %t8, ptr %t9
"
            ),
            "{program}"
        );
        // The copy a loop walks is freed alone: its items are the list's.
        assert!(program.contains("done1:\n  call void @zeno_list_drop(ptr %t12)\n"), "{program}");
        // The end of the program frees each list with its items.
        assert!(
            program.ends_with(
                "  %t36 = load ptr, ptr @z_grid
  call void @zeno_list_drop_2_Point(ptr %t36)
  %t37 = load ptr, ptr @z_points
  call void @zeno_list_drop_1_Point(ptr %t37)
  ret void"
            ),
            "{program}"
        );
    }

    #[test]
    fn the_ir_for_an_enum_value_that_has_one_owner() {
        // docs/enums-and-none.md, in IR.
        let ir = compile_no_collector(crate::emit_c::tests::ENUM_C).unwrap();
        // The function that makes a case keeps a copy of its text.
        let make = ir.split("define internal ptr @zeno.new.Shape.Named(ptr %f0, ptr %f1) {\n").nth(1).expect("the function");
        assert!(make.split("\n}\n").next().unwrap().contains("  %t3 = call ptr @zeno_text_keep(ptr %f0)\n  store ptr %t3, ptr %t2\n"), "{ir}");
        // Freeing a value goes by its case. One that carries nothing is
        // a fixed value and is left alone.
        let drop = ir.split("define internal void @zeno.drop.Shape(ptr %self) {\n").nth(1).expect("the function");
        let drop = drop.split("\n}\n").next().unwrap();
        assert_eq!(
            drop,
            "entry:
  %t1 = icmp eq ptr %self, null
  br i1 %t1, label %gone1, label %there1

gone1:
  ret void

there1:
  %t2 = load i64, ptr %self
  switch i64 %t2, label %bare [ i64 1, label %case1 i64 2, label %case2 ]

case1:
  call void @zeno_free(ptr %self)
  ret void

case2:
  %t3 = getelementptr %z_Shape.Named, ptr %self, i32 0, i32 1
  %t4 = load ptr, ptr %t3
  call void @zeno_text_drop(ptr %t4)
  %t5 = getelementptr %z_Shape.Named, ptr %self, i32 0, i32 2
  %t6 = load ptr, ptr %t5
  call void @zeno_list_drop(ptr %t6)
  call void @zeno_free(ptr %self)
  ret void

bare:
  ret void"
        );
        let program = ir.split("define internal void @zeno_program() noinline {\n").nth(1).expect("the program");
        let program = program.split("\n}\n").next().unwrap();
        // A new value frees the old one, and `none` is null.
        assert!(
            program.ends_with(
                "end1:
  %t13 = load ptr, ptr @z_shape
  call void @zeno.drop.Shape(ptr %t13)
  store ptr @zeno.case.Shape.Dot, ptr @z_shape
  store ptr null, ptr @z_last
  %t14 = call ptr @zeno.new.Shape.Circle(double 1.0)
  %t15 = load ptr, ptr @z_last
  call void @zeno.drop.Shape(ptr %t15)
  store ptr %t14, ptr @z_last
  %t16 = load ptr, ptr @z_last
  call void @zeno.drop.Shape(ptr %t16)
  %t17 = load ptr, ptr @z_shape
  call void @zeno.drop.Shape(ptr %t17)
  ret void"
            ),
            "{program}"
        );
    }

    #[test]
    fn the_ir_for_a_function_value_that_has_one_owner() {
        // docs/functions-and-errors.md, in IR.
        let ir = compile_no_collector(crate::emit_c::tests::FUNCTION_C).unwrap();
        // A function of the program is a fixed value with no `drop`.
        assert!(
            ir.contains("@zeno.value.double = internal constant { ptr, ptr, ptr } { ptr @zeno.wrap.double, ptr @.str.3, ptr null }\n"),
            "{ir}"
        );
        // One written where a value goes is a block: code, name, what
        // frees it, and then what it uses. The text is a copy.
        let make = ir.split("define internal ptr @z_make_tagger(ptr %arg.tag, i64 %arg.width) {\n").nth(1).expect("the function");
        assert_eq!(
            make.split("\n}\n").next().unwrap(),
            "entry:
  %t1 = call ptr @zeno_alloc(i64 ptrtoint (ptr getelementptr ({ ptr, ptr, ptr, ptr, i64 }, ptr null, i32 1) to i64))
  store ptr @zeno.fn.2, ptr %t1
  %t2 = getelementptr { ptr, ptr, ptr, ptr, i64 }, ptr %t1, i32 0, i32 1
  store ptr @.str.4, ptr %t2
  %t3 = getelementptr { ptr, ptr, ptr, ptr, i64 }, ptr %t1, i32 0, i32 3
  %t4 = call ptr @zeno_text_keep(ptr %arg.tag)
  store ptr %t4, ptr %t3
  %t5 = getelementptr { ptr, ptr, ptr, ptr, i64 }, ptr %t1, i32 0, i32 4
  store i64 %arg.width, ptr %t5
  %t6 = getelementptr { ptr, ptr, ptr, ptr, i64 }, ptr %t1, i32 0, i32 2
  store ptr @zeno.fn.drop.2, ptr %t6
  ret ptr %t1"
        );
        let drop = ir.split("define internal void @zeno.fn.drop.2(ptr %self) {\n").nth(1).expect("the function");
        assert_eq!(
            drop.split("\n}\n").next().unwrap(),
            "entry:
  %t1 = getelementptr { ptr, ptr, ptr, ptr, i64 }, ptr %self, i32 0, i32 3
  %t2 = load ptr, ptr %t1
  call void @zeno_text_drop(ptr %t2)
  call void @zeno_free(ptr %self)
  ret void"
        );
        // A new value frees the old one, whichever kind each is.
        let program = ir.split("define internal void @zeno_program() noinline {\n").nth(1).expect("the program");
        assert_eq!(
            program.split("\n}\n").next().unwrap(),
            "entry:
  %t1 = call ptr @z_make_tagger(ptr @.str.1, i64 4)
  store ptr %t1, ptr @z_tagger
  %t2 = call ptr @z_make_tagger(ptr @.str.2, i64 2)
  %t3 = load ptr, ptr @z_tagger
  call void @zeno_closure_drop(ptr %t3)
  store ptr %t2, ptr @z_tagger
  store ptr @zeno.value.double, ptr @z_current
  %t4 = call ptr @zeno_alloc(i64 ptrtoint (ptr getelementptr ({ ptr, ptr, ptr }, ptr null, i32 1) to i64))
  store ptr @zeno.fn.1, ptr %t4
  %t5 = getelementptr { ptr, ptr, ptr }, ptr %t4, i32 0, i32 1
  store ptr @.str.4, ptr %t5
  %t6 = getelementptr { ptr, ptr, ptr }, ptr %t4, i32 0, i32 2
  store ptr @zeno_closure_release, ptr %t6
  %t7 = load ptr, ptr @z_current
  call void @zeno_closure_drop(ptr %t7)
  store ptr %t4, ptr @z_current
  %t8 = load ptr, ptr @z_current
  call void @zeno_closure_drop(ptr %t8)
  %t9 = load ptr, ptr @z_tagger
  call void @zeno_closure_drop(ptr %t9)
  ret void"
        );
        // Where there is a collector, a function value has no third word.
        let shared = compile(crate::emit_c::tests::FUNCTION_C).unwrap();
        assert!(shared.contains("@zeno.value.double = internal constant { ptr, ptr } { ptr @zeno.wrap.double,"), "{shared}");
    }

    #[test]
    fn the_ir_for_kept_text() {
        // docs/text.md, in IR.
        let ir = compile_no_collector(
            "function label(channel: int) -> str {
    return f\"ch{channel}\"
}

var best = \"none yet\"
for channel in range(3) {
    let name = label(channel)
    if channel == 1 {
        best = name
    }
}
print(best)
",
        )
        .expect("it compiles");
        // A function's answer travels in the text room: the runtime moves
        // it down to where the room's top was when the `return` began.
        let label = "define internal ptr @z_label(i64 %arg.channel) {
entry:
  %mark.1 = load i64, ptr @zeno_text_top
  %t1 = call ptr @zeno_str_int(i64 %arg.channel)
  %t2 = call ptr (i32, ...) @zeno_join(i32 2, ptr @.str.2, ptr %t1)
  %t3 = call ptr @zeno_text_return(i64 %mark.1, ptr %t2)
  ret ptr %t3
}";
        assert!(ir.contains(label), "{ir}");
        // `name` keeps a copy of the answer, in a slot, and the end of
        // each trip frees what the slot holds.
        let trip = "do1:
  %mark.1 = load i64, ptr @zeno_text_top
  %t3 = call ptr @z_label(i64 %t1)
  %t4 = call ptr @zeno_text_keep(ptr %t3)
  store ptr %t4, ptr %var.name
  store i64 %mark.1, ptr @zeno_text_top
  %t5 = icmp eq i64 %t1, 1
  br i1 %t5, label %then2, label %end2

then2:
  %t6 = load ptr, ptr %var.name
  %t7 = call ptr @zeno_text_keep(ptr %t6)
  %t8 = load ptr, ptr @z_best
  call void @zeno_text_drop(ptr %t8)
  store ptr %t7, ptr @z_best
  br label %end2

end2:
  %t9 = load ptr, ptr %var.name
  call void @zeno_text_drop(ptr %t9)
  br label %next1
";
        assert!(ir.contains(trip), "{ir}");
        // `best` can be changed by a function, so reading it lends the
        // statement a copy. The end of the file frees what it holds.
        let end = "done1:
  %mark.2 = load i64, ptr @zeno_text_top
  %t12 = load ptr, ptr @z_best
  %t13 = call ptr @zeno_text_lend(ptr %t12)
  call void (i32, ...) @zeno_print(i32 1, ptr %t13)
  store i64 %mark.2, ptr @zeno_text_top
  %t14 = load ptr, ptr @z_best
  call void @zeno_text_drop(ptr %t14)
  ret void
}";
        assert!(ir.contains(end), "{ir}");
        // Text read from a field and stored at once is copied straight
        // from the field: nothing is lent on the way.
        let direct = compile_no_collector(
            "owned class Tag {\n    name: str\n}\nlet tag = Tag(\"a\" + \"b\")\nlet copy = tag.name\nprint(copy)\n",
        )
        .expect("it compiles");
        assert!(direct.contains("call ptr @zeno_text_keep(ptr %t"), "{direct}");
        assert!(!direct.contains("call ptr @zeno_text_lend("), "{direct}");
        // With a collector none of it is there: text is shared, as ever.
        let shared = compile("var best = \"a\"\nbest = best + \"b\"\nprint(best)").expect("it compiles");
        for gone in ["zeno_text_keep", "zeno_text_drop", "zeno_text_lend", "zeno_text_return"] {
            assert!(!shared.contains(gone), "{gone}");
        }
    }

    #[test]
    fn with_no_collector_an_error_is_freed_by_the_except_that_catches_it() {
        let source = "function check(n: int) -> int raises ValueError {
    if n > 2 {
        raise ValueError(f\"{n} is too big\")
    }
    return n
}
for n in [1, 5] {
    try {
        print(check(n))
    } except ValueError as e {
        if n > 4 {
            print(e)
            continue
        }
        print(\"small\")
    }
    print(\"fine\")
}
";
        let ir = compile_no_collector(source).expect("it compiles");
        // Once where the `except` block ends, and once where `continue`
        // leaves it early.
        assert_eq!(ir.matches("call void @zeno_error_drop(ptr %t").count(), 2, "{ir}");
        // The list the loop walks is the loop's own, and is freed when
        // the loop is over.
        assert_eq!(ir.matches("call void @zeno_list_drop(ptr %t").count(), 1, "{ir}");
        let Some((printed, errors, finished)) = build_and_run_as("error-drop", &ir, Build::NoCollector, &["-DZENO_MEMORY_REPORT"])
        else {
            return;
        };
        assert!(finished, "{errors}");
        assert_eq!(printed, "1\nfine\n5 is too big\n");
        assert!(errors.contains("no collector, 0 blocks left at the end\n"), "{errors}");
    }

    #[test]
    fn functions_as_values_match() {
        same_as_interpreter(
            "closures",
            "function make_counter(start: int) -> function() -> int {\n    var count = start\n\
             \x20   return function() -> int {\n        count = count + 1\n        return count\n    }\n}\n\
             let a = make_counter(10)\nlet b = make_counter(100)\n\
             print(a(), a(), b(), a(), b())\n\
             function twice(f: function(int) -> int, x: int) -> int { return f(f(x)) }\n\
             function inc(n: int) -> int { return n + 1 }\n\
             function adder(k: int) -> function(int) -> int {\n    return function(n: int) -> int { return n + k }\n}\n\
             let add5 = adder(5)\n\
             print(twice(inc, 5), twice(function(n: int) -> int { return n * n }, 3), add5(1), adder(2)(3), inc)\n\
             let nums = [1, 2, 3, 4, 5, 6]\n\
             print(nums.map(function(n: int) -> int { return n * 2 }))\n\
             print(nums.filter(function(n: int) -> bool { return n % 2 == 0 }))\n\
             print(nums.reduce(function(sum: int, n: int) -> int { return sum + n }, 0), nums.map(inc).map(add5))\n\
             function outer() -> int {\n    var total = 0\n    let bump = function(by: int) { total = total + by }\n\
             \x20   for i in range(5) { bump(i) }\n\
             \x20   let sizes = [\"x\", \"yy\"].map(function(s: str) -> int {\n        total = total + 1\n        return len(s)\n    })\n\
             \x20   print(sizes, total)\n    return total\n}\n\
             print(outer())\n\
             class Button {\n    label: str\n    on_click: function(str) -> str\n}\n\
             let button = Button(\"ok\", function(who: str) -> str { return \"clicked by \" + who })\n\
             print(button.on_click(\"me\"), button.label)\n\
             let all: list[function(int) -> int] = [inc, add5, function(n: int) -> int { return -n }]\n\
             for f in all { print(f(10)) }\n\
             print(all[2](7))\n\
             if true {\n    var local = 3\n    let peek = function() -> int {\n        local = local + 1\n        return local\n    }\n\
             \x20   print(peek(), peek(), local)\n}\n",
        );
    }

    #[test]
    fn units_and_imports_match() {
        same_as_interpreter(
            "units",
            "import math\nfrom math import floor, pi\n\
             print(500 ms + 1 s, (2 s).ms, 3 V * 2, 10 s / 4 s, 6 V / 2, 1 s > 900 ms, abs(-3 ms), -(2 s))\n\
             let wait: ms = 250 ms\n\
             print(wait, wait * 4 == 1 s, wait.s, f\"{wait}\", [wait, 1 ms])\n\
             print(math.sqrt(16.0), floor(2.7), math.ceil(2.1), math.sin(0.0), math.cos(0.0), pi > 3.14, math.pi < 3.15)\n\
             print(math.sqrt(-1.0))\n",
        );
    }

    #[test]
    fn the_ir_for_a_class() {
        let ir = compile(
            "class Point {\n    x: float\n    y: float\n    function width(self) -> float { return self.x }\n}\n\
             let p = Point(1.0, 2.0)\np.y = 3.0\nprint(p.width())\n",
        )
        .expect("it compiles");
        // A class is a struct type, and a field is found with
        // `getelementptr`.
        assert!(ir.contains("%z_Point = type { double, double }"), "{ir}");
        let method = "define internal double @z_Point.width(ptr %arg.self) {\n\
                      entry:\n\
                      \x20 %t1 = getelementptr %z_Point, ptr %arg.self, i32 0, i32 0\n\
                      \x20 %t2 = load double, ptr %t1\n\
                      \x20 ret double %t2\n\
                      }\n";
        assert!(ir.contains(method), "{ir}");
        assert!(ir.contains("getelementptr %z_Point, ptr %t2, i32 0, i32 1\n  store double 3.0, ptr %t3"), "{ir}");
    }

    #[test]
    fn the_runtime_can_be_linked_to() {
        let (ordinary, firmware, bare) = (runtime(Build::Ordinary), runtime(Build::Firmware), runtime(Build::NoCollector));
        let has = |runtime: &str, name: &str| runtime.contains(&format!(" {name}(")) || runtime.contains(&format!("*{name}("));
        // What only some of the three kinds of build call.
        let jumps = ["zeno_raise", "zeno_raise_again", "zeno_caught", "zeno_try_size", "zeno_try_open", "zeno_try_close"];
        let values = ["zeno_note", "zeno_is"];
        let with_collector = ["zeno_mark_words"];
        let without_collector = [
            "zeno_error_drop",
            "zeno_text_keep",
            "zeno_text_drop",
            "zeno_text_lend",
            "zeno_text_take",
            "zeno_text_return",
            "zeno_list_drop_text",
            "zeno_list_copy_text",
            "zeno_list_keep_text",
            "zeno_error_of",
            "zeno_closure_drop",
            "zeno_closure_release",
        ];
        for runtime in [&ordinary, &firmware, &bare] {
            assert!(!runtime.lines().any(|line| line.starts_with("static ")));
        }
        // Everything the IR may call is there to be found.
        // The functions of a test build are in its own part of the
        // runtime, and in no other program's (decision 60.2).
        let tests = tests_runtime(&crate::test_build::TestBuild::default(), Build::Ordinary);
        for (name, _) in DECLARED {
            if name.starts_with("zeno_test") {
                assert!(has(&tests, name) && !has(&ordinary, name) && !has(&firmware, name) && !has(&bare, name), "{name}");
            } else if name.starts_with("zeno_") {
                assert_eq!(has(&ordinary, name), !values.contains(name) && !without_collector.contains(name), "{name}");
                assert_eq!(has(&firmware, name), !jumps.contains(name) && !without_collector.contains(name), "{name}");
                assert_eq!(has(&bare, name), !jumps.contains(name) && !with_collector.contains(name), "{name}");
            }
        }
        // A firmware build has nothing to jump with, and a build with no
        // collector has no collector.
        for runtime in [&firmware, &bare] {
            assert!(!runtime.contains("setjmp") && !runtime.contains("longjmp"));
        }
        for gone in ["zeno_collect", "zeno_roots", "zeno_stack_base", "zeno_pages"] {
            assert!(ordinary.contains(gone) && firmware.contains(gone) && !bare.contains(gone), "{gone}");
        }
        assert!(bare.contains("char zeno_text_room[") && bare.contains("size_t zeno_text_top"));
        // And it takes its memory from the pool, where the other two take
        // it from the heap.
        assert_eq!(asks_the_heap(&bare), None);
        assert!(bare.contains("char bytes[ZENO_POOL];"));
        assert_eq!(asks_the_heap(&ordinary), Some("malloc"));
        assert_eq!(asks_the_heap(&firmware), Some("malloc"));
    }

    #[test]
    fn the_ir_for_a_small_function() {
        // A `let` and a parameter are values. A `var` has a slot.
        let ir = compile(
            "function scale(n: int, by: float) -> float {\n\
             \x20   let twice = by * 2.0\n\
             \x20   var result = twice\n\
             \x20   if n > 0 { result = result * 0.5 }\n\
             \x20   return result\n\
             }\n\
             print(scale(1, 1.5))\n",
        )
        .expect("it compiles");
        let function = "define internal double @z_scale(i64 %arg.n, double %arg.by) {\n\
                        entry:\n\
                        \x20 %var.result = alloca double\n\
                        \x20 %t1 = fmul double %arg.by, 2.0\n\
                        \x20 store double %t1, ptr %var.result\n\
                        \x20 %t2 = icmp sgt i64 %arg.n, 0\n\
                        \x20 br i1 %t2, label %then1, label %end1\n\
                        \n\
                        then1:\n\
                        \x20 %t3 = load double, ptr %var.result\n\
                        \x20 %t4 = fmul double %t3, 0.5\n\
                        \x20 store double %t4, ptr %var.result\n\
                        \x20 br label %end1\n\
                        \n\
                        end1:\n\
                        \x20 %t5 = load double, ptr %var.result\n\
                        \x20 ret double %t5\n\
                        }\n";
        assert!(ir.contains(function), "{ir}");
    }

    #[test]
    fn the_ir_for_blink() {
        let ir = compile(include_str!("../examples/blink.zeno")).expect("it compiles");
        // A pin is its number, and each thing done to it is one call.
        // 500 ms reaches the board as seconds.
        let body = "do1:\n\
                    \x20 call void @zeno_pin_toggle(i64 13)\n\
                    \x20 %t1 = sitofp i64 500 to double\n\
                    \x20 %t2 = fdiv double %t1, 1000.0\n\
                    \x20 call void @zeno_sleep(double %t2)\n\
                    \x20 br label %while1\n";
        assert!(ir.contains(body), "{ir}");
    }

    #[test]
    fn the_ir_for_a_sized_field() {
        let source = "class Packet {\n    kind: u8\n    length: u16\n    offset: i8\n}\n\
                      let p = Packet(3, 1024, -1)\nprint(p.kind + p.offset, u8.wrap(p.length), u8(p.length))\n";
        let ir = compile(source).expect("it compiles");
        // Four bytes of fields, where three plain ints would take 24.
        assert!(ir.contains("%z_Packet = type { i8, i16, i8 }"), "{ir}");
        // Going in, the i64 is cut down to the size of the field.
        assert!(ir.contains("  %t3 = trunc i64 %f0 to i8\n  store i8 %t3, ptr %t2\n"), "{ir}");
        // Coming out, it is made an i64 again: with zeros for a u8, and
        // with copies of the sign bit for an i8.
        assert!(ir.contains("  %t4 = load i8, ptr %t3\n  %t5 = zext i8 %t4 to i64\n"), "{ir}");
        assert!(ir.contains("  %t8 = load i8, ptr %t7\n  %t9 = sext i8 %t8 to i64\n"), "{ir}");
        // `u8.wrap` is the same two instructions, and `u8( )` asks the runtime.
        assert!(ir.contains(" = trunc i64 %t"), "{ir}");
        assert!(ir.contains("call i64 @zeno_fit(i64 %t"), "{ir}");
        assert!(ir.contains(", i64 0, i64 255, ptr @.str"), "{ir}");
    }

    #[test]
    fn the_ir_for_a_generic() {
        let source = "function first[T](items: list[T]) -> T {\n    return items[0]\n}\n\
                      class Cell[T] {\n    value: T\n    function get(self) -> T {\n        return self.value\n    }\n}\n\
                      function unused[T](x: T) -> T {\n    return x\n}\n\
                      let small: u8 = 7\n\
                      print(first([1, 2]), first([\"a\"]), first([3]))\n\
                      print(Cell(1.5).get(), Cell(small))\n";
        let ir = compile(source).expect("it compiles");
        // One function for each type `first` is called with. IR allows
        // any name in quotes, so the copies keep the names Zeno gives them.
        assert!(ir.contains("define internal i64 @\"z_first[int]\"(ptr %arg.items) {"), "{ir}");
        assert!(ir.contains("define internal ptr @\"z_first[str]\"(ptr %arg.items) {"), "{ir}");
        assert_eq!(ir.matches("define internal i64 @\"z_first[int]\"").count(), 1, "{ir}");
        // One struct type for each `Cell`, as big as what it holds.
        assert!(ir.contains("%\"z_Cell[float]\" = type { double }"), "{ir}");
        assert!(ir.contains("%\"z_Cell[u8]\" = type { i8 }"), "{ir}");
        assert!(ir.contains("define internal double @\"z_Cell[float].get\"(ptr %arg.self) {"), "{ir}");
        // Nothing for the generic itself, or for one that nothing calls.
        assert!(!ir.contains("@z_first(") && !ir.contains("%z_Cell ") && !ir.contains("z_unused"), "{ir}");
    }

    #[test]
    fn what_is_not_compiled_yet_says_so() {
        // A method of an object that no variable holds says what to write.
        let method = "class Box {\n    function size(self) -> int { return 1 }\n}\nlet f = Box().size\n";
        assert_eq!(
            compile(method).expect_err("it should be refused"),
            "`zeno build --llvm` cannot compile a method used as a value of an object that no variable holds, `.size`, \
             yet. Put the object in a `let` first and use `.size` of that: then it builds. `zeno run` can still run \
             this program"
        );
        let refused = [
            (
                "function outer() -> int {\n    function id[T](x: T) -> T {\n        return x\n    }\n    return id(4)\n}\nprint(outer())\n",
                "a generic function inside a function or block",
            ),
        ];
        for (source, what) in refused {
            let message = compile(source).expect_err("it should be refused");
            assert_eq!(
                message,
                format!("`zeno build --llvm` cannot compile {what} yet. `zeno run` can still run this program"),
                "\n{source}"
            );
        }
    }

    // ------------------------------------------- Memory by hand (docs/unsafe.md)

    #[test]
    fn the_ir_for_memory_by_hand() {
        let ir = compile(crate::emit_c::tests::BY_HAND_C).unwrap();
        // A pointer is `ptr`, and an item is found with `getelementptr`
        // at the size of the item. Reading makes it 64 bits wide again.
        let sum = ir.split("define internal i64 @z_sum(ptr %arg.bytes, i64 %arg.count) {\n").nth(1).expect("the function");
        assert!(
            sum.contains(
                "do2:
  %t3 = load i64, ptr %var.total
  %t4 = getelementptr i8, ptr %arg.bytes, i64 %t1
  %t5 = load i8, ptr %t4
  %t6 = zext i8 %t5 to i64
"
            ),
            "{sum}"
        );
        // The block is a label going in and a label coming out.
        assert!(sum.contains("  br label %unsafe1\n\nunsafe1:\n") && sum.contains("  br label %safe1\n\nsafe1:\n"), "{sum}");
        // A register is its address, and its size goes with each call.
        let set = ir.split("define internal void @z_set_bit(i64 %arg.port, i64 %arg.bit) {\n").nth(1).expect("the function");
        assert_eq!(
            set.split("\n}\n").next().unwrap(),
            "entry:
  br label %unsafe1

unsafe1:
  %t1 = call i64 @zeno_register_read(i64 %arg.port, i32 signext 32)
  %t2 = call i64 @zeno_shl(i64 1, i64 %arg.bit, i32 15, i32 38)
  %t3 = or i64 %t1, %t2
  %t4 = call i64 @zeno_fit(i64 %t3, i64 0, i64 4294967295, ptr @.str.2, i32 15, i32 20)
  call void @zeno_register_write(i64 %arg.port, i32 signext 32, i64 %t4)
  br label %safe1

safe1:
  ret void"
        );
        let program = ir.split("define internal void @zeno_program() noinline {\n").nth(1).expect("the program");
        // `allocate`: how many, and the size of one.
        assert!(program.contains("  %t2 = call ptr @zeno_allocate(i64 2, i64 2, i32 20, i32 13)\n"), "{program}");
        // A write is cut down to the size of the item. A signed item is
        // read back with its sign.
        assert!(
            program.contains(
                "  %t17 = load ptr, ptr @z_words
  %t18 = getelementptr i16, ptr %t17, i64 1
  %t19 = trunc i64 -300 to i16
  store i16 %t19, ptr %t18
"
            ),
            "{program}"
        );
        assert!(program.contains("  %t25 = load i16, ptr %t24\n  %t26 = sext i16 %t25 to i64\n"), "{program}");
        // `bytes + 1` is an address one item further on.
        assert!(
            program.contains("  %t21 = getelementptr i8, ptr %t20, i64 1\n  %t22 = call i64 @z_sum(ptr %t21, i64 3)\n"),
            "{program}"
        );
        assert!(program.contains("  call void @zeno_release(ptr %t29)\n"), "{program}");
    }

    #[test]
    fn the_memory_report_counts_what_was_made_by_hand() {
        let source = "let a = allocate(u8, 16)\nlet b = allocate(int, 4)\nunsafe { free(a) }\nprint(b)";
        let program = crate::parser::parse(crate::lexer::lex(source).unwrap()).unwrap();
        let (errors, types) = crate::checker::check_with_types(&program, true);
        assert!(errors.is_empty(), "the test program has mistakes: {:?}", errors[0].message);
        // `true`: this build is asked for the report, as `--memory` does.
        let ir = emit(&program, &types, "test.zeno", Build::Ordinary, true).expect("it compiles");
        let Some((printed, errors, finished)) = build_and_run_as("by-hand-report", &ir, Build::Ordinary, &["-DZENO_MEMORY_REPORT"])
        else {
            return;
        };
        assert!(finished && printed == "<pointer[int]>\n", "{printed}{errors}");
        assert!(errors.ends_with("by hand: 2 blocks from `allocate`, 1 never freed\n"), "{errors}");
    }
}
