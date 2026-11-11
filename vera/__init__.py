"""VERA: Verified Relational Architecture (v1.1 implementation).

A deterministic, transactional language + verifier + runtime.

Sub-packages / modules:
    errors      -- diagnostics and error codes
    types       -- fixed-width scalar types and SoA memory layout
    ast_nodes   -- abstract syntax tree
    lexer       -- tokenizer
    parser      -- recursive-descent parser
    semantic    -- symbol tables, type checking, access sets, WCET
    smt_emit    -- AST -> SMT-LIB 2.6 (QF_ABV) lowering
    verifier    -- three-proof-obligation driver + counterexample lifting
    runtime     -- reference SoA execution engine
    projection  -- State & Affordance Frame construction
    codegen_rust-- Target C: zero-alloc Rust/LLVM emission
    codegen_gpu  -- Target B: CUDA / WGSL emission
    fuzz        -- reference trajectory-blasting fuzzer
    cli         -- command line interface
"""

__version__ = "1.1.0"

__all__ = ["__version__"]
