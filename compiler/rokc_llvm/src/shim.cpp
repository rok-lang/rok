// C ABI over the statically linked LLVM / MLIR / lld that rokc uses.
// Every entry point reports failure as text in `err`; buffers handed out
// are malloc'd and released with rok_llvm_free.

#include "lld/Common/Driver.h"
#include "llvm/ADT/EquivalenceClasses.h"
#include "llvm/ADT/StringRef.h"
#include "llvm/ADT/StringSet.h"
#include "llvm/Analysis/ModuleSummaryAnalysis.h"
#include "llvm/Bitcode/BitcodeReader.h"
#include "llvm/Bitcode/BitcodeWriter.h"
#include "llvm/IR/DIBuilder.h"
#include "llvm/IR/DebugInfo.h"
#include "llvm/IR/DiagnosticInfo.h"
#include "llvm/IR/DiagnosticPrinter.h"
#include "llvm/IR/LLVMContext.h"
#include "llvm/IR/LegacyPassManager.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/Verifier.h"
#include "llvm/IRReader/IRReader.h"
#include "llvm/LTO/LTO.h"
#include "llvm/Linker/Linker.h"
#include "llvm/MC/TargetRegistry.h"
#include "llvm/Object/ArchiveWriter.h"
#include "llvm/Object/COFFImportFile.h"
#include "llvm/Object/COFFModuleDefinition.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/ProfileData/InstrProfReader.h"
#include "llvm/ProfileData/InstrProfWriter.h"
#include "llvm/Support/Caching.h"
#include "llvm/Support/CommandLine.h"
#include "llvm/ProfileData/ProfileCommon.h"
#include "llvm/Support/Format.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/PGOOptions.h"
#include "llvm/Support/VirtualFileSystem.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/SourceMgr.h"
#include "llvm/Support/TargetSelect.h"
#include "llvm/Support/ThreadPool.h"
#include "llvm/Support/Threading.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Target/TargetMachine.h"
#include "llvm/Target/TargetOptions.h"
#include "llvm/TargetParser/Host.h"
#include "llvm/TargetParser/Triple.h"
#include "llvm/Transforms/IPO/AlwaysInliner.h"
#include "llvm/Transforms/InstCombine/InstCombine.h"
#include "llvm/Transforms/Scalar/SimplifyCFG.h"
#include "llvm/Transforms/Utils/Cloning.h"
#include "llvm/Support/LLVMDriver.h"
#include "mlir/IR/BuiltinOps.h"
#include "mlir/IR/Diagnostics.h"
#include "mlir/IR/MLIRContext.h"
#include "mlir/Conversion/FuncToLLVM/ConvertFuncToLLVM.h"
#include "mlir/Conversion/Passes.h"
#include "mlir/Dialect/Arith/IR/Arith.h"
#include "mlir/Dialect/ControlFlow/IR/ControlFlow.h"
#include "mlir/Dialect/DLTI/DLTI.h"
#include "mlir/Dialect/Func/IR/FuncOps.h"
#include "mlir/Dialect/Index/IR/IndexDialect.h"
#include "mlir/Dialect/LLVMIR/LLVMDialect.h"
#include "mlir/Dialect/Math/IR/Math.h"
#include "mlir/Dialect/SCF/IR/SCF.h"
#include "mlir/Dialect/UB/IR/UBOps.h"
#include "mlir/Interfaces/CastInterfaces.h"
#include "mlir/Parser/Parser.h"
#include "mlir/Pass/PassManager.h"
#include "mlir/Pass/PassRegistry.h"
#include "mlir/Target/LLVMIR/Dialect/Builtin/BuiltinToLLVMIRTranslation.h"
#include "mlir/Target/LLVMIR/Dialect/LLVMIR/LLVMToLLVMIRTranslation.h"
#include "mlir/Target/LLVMIR/Export.h"
#include "mlir/Transforms/Passes.h"

#include <algorithm>
#include <atomic>
#include <functional>
#include <cinttypes>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

LLD_HAS_DRIVER(macho)
LLD_HAS_DRIVER(elf)
LLD_HAS_DRIVER(coff)
LLD_HAS_DRIVER(wasm)

int dsymutil_main(int argc, char **argv, const llvm::ToolContext &);

extern "C" {

struct RokBuf {
  char *ptr;
  size_t len;
};

// opt: 0..3, 's' (Os), 'z' (Oz). emit: 0 LLVM IR text, 1 asm, 2 object.
struct RokCompile {
  const char *triple;
  const char *cpu;
  const char *features;
  int opt;
  int emit;
  // rok_tls (the TLS block) uses the initial-exec
  // TLS model (spec 8.3.8).
  int tls_initial_exec;
  // A textual MLIR pass pipeline on builtin.module, mlir-opt syntax
  // without the anchor: "canonicalize,cse,convert-scf-to-cf,...".
  const char *mlir_pipeline;
  // Objects to split the module into, optimized and compiled in parallel
  // (`out` holds this many); 1 for one object, an IR text or assembly.
  size_t parts;
  // IR PGO: instrument, the raw profile written to this file name
  // pattern; or optimize with this indexed profile. "" for neither.
  const char *profile_generate;
  const char *profile_use;
};

void rok_llvm_free(RokBuf *b) {
  free(b->ptr);
  b->ptr = nullptr;
  b->len = 0;
}

} // extern "C"

namespace {

void put(RokBuf *b, llvm::StringRef s) {
  b->ptr = static_cast<char *>(malloc(s.size() + 1));
  memcpy(b->ptr, s.data(), s.size());
  b->ptr[s.size()] = 0;
  b->len = s.size();
}

int fail(RokBuf *err, llvm::StringRef msg) {
  put(err, msg);
  return 1;
}

void init_llvm() {
  static std::once_flag once;
  std::call_once(once, [] {
    llvm::InitializeAllTargetInfos();
    llvm::InitializeAllTargets();
    llvm::InitializeAllTargetMCs();
    llvm::InitializeAllAsmPrinters();
    llvm::InitializeAllAsmParsers();
  });
}

// Dialects rokc lowers to (spec 8.7.1), with what lowering them needs (ub
// and DLTI for the LLVM conversions). No others: the linker leaves the
// rest of MLIR out.
void register_dialects(mlir::DialectRegistry &r) {
  using namespace mlir;
  r.insert<arith::ArithDialect, cf::ControlFlowDialect, DLTIDialect,
           func::FuncDialect, index::IndexDialect, LLVM::LLVMDialect,
           math::MathDialect, scf::SCFDialect, ub::UBDialect>();
  builtin::registerCastOpInterfaceExternalModels(r);
  // For convert-to-llvm.
  arith::registerConvertArithToLLVMInterface(r);
  cf::registerConvertControlFlowToLLVMInterface(r);
  registerConvertFuncToLLVMInterface(r);
  index::registerConvertIndexToLLVMInterface(r);
  registerConvertMathToLLVMInterface(r);
  ub::registerConvertUBToLLVMInterface(r);
  registerBuiltinDialectTranslation(r);
  registerLLVMDialectTranslation(r);
}

// Pass names for pipelines, as mlir-opt spells them.
void register_passes() {
  using namespace mlir;
  registerTransformsPasses();
  registerArithToLLVMConversionPass();
  registerConvertControlFlowToLLVMPass();
  registerConvertFuncToLLVMPass();
  registerConvertIndexToLLVMPass();
  registerConvertMathToLibmPass();
  registerConvertMathToLLVMPass();
  registerReconcileUnrealizedCastsPass();
  registerSCFToControlFlowPass();
}

void init_passes() {
  static std::once_flag once;
  std::call_once(once, register_passes);
}

llvm::TargetOptions target_options(const llvm::Triple &triple) {
  llvm::TargetOptions opts;
  opts.UseInitArray = true;
  opts.EmulatedTLS = triple.hasDefaultEmulatedTLS();
  // Lets the link drop what nothing references; Mach-O strips by symbol.
  opts.FunctionSections = opts.DataSections = !triple.isOSBinFormatMachO();
  return opts;
}

llvm::CodeGenOptLevel codegen_level(int opt) {
  switch (opt) {
  case 0:
    return llvm::CodeGenOptLevel::None;
  case 1:
    return llvm::CodeGenOptLevel::Less;
  case 3:
    return llvm::CodeGenOptLevel::Aggressive;
  default:
    return llvm::CodeGenOptLevel::Default;
  }
}

std::unique_ptr<llvm::TargetMachine>
target_machine(const RokCompile &o, std::string &error) {
  init_llvm();
  llvm::Triple triple(o.triple);
  const llvm::Target *t = llvm::TargetRegistry::lookupTarget(triple, error);
  if (!t)
    return nullptr;
  std::unique_ptr<llvm::TargetMachine> tm(t->createTargetMachine(
      triple, o.cpu, o.features, target_options(triple), llvm::Reloc::PIC_,
      std::nullopt, codegen_level(o.opt)));
  if (!tm) {
    error = "cannot create a target machine for " + triple.str();
    return nullptr;
  }
  // SelectionDAG at every level: at -O0 AArch64 would pick GlobalISel,
  // which falls back per function and lays out sub-word stack aggregates
  // differently.
  tm->setGlobalISel(false);
  return tm;
}

// The indexed profile `path` has context-sensitive records (as clang
// checks for -fprofile-use).
bool has_cs_profile(const char *path) {
  auto fs = llvm::vfs::getRealFileSystem();
  auto r = llvm::IndexedInstrProfReader::create(path, *fs);
  if (!r) {
    llvm::consumeError(r.takeError());
    return false;
  }
  return (*r)->hasCSIRLevelProfile();
}

// `rok.alloc` is an allocation function to the optimizer (fresh, zeroed,
// unaliased), so non-escaping objects die with their stores; it is inlined
// back to the bump path at the end of the pipeline.
const char *const ALLOC = "rok.alloc";

struct InlineAlloc : llvm::PassInfoMixin<InlineAlloc> {
  llvm::PreservedAnalyses run(llvm::Module &m, llvm::ModuleAnalysisManager &) {
    llvm::Function *f = m.getFunction(ALLOC);
    if (!f)
      return llvm::PreservedAnalyses::all();
    f->removeFnAttr(llvm::Attribute::NoInline);
    f->removeFnAttr(llvm::Attribute::AllocKind);
    f->addFnAttr(llvm::Attribute::AlwaysInline);
    return llvm::PreservedAnalyses::none();
  }
};

// What clang -O<n> sets up for an IR input
// (shouldEnableVectorizerAtOLevel, -funroll-loops).
llvm::PipelineTuningOptions tuning(int opt) {
  llvm::PipelineTuningOptions pto;
  bool vec = opt >= 2 || opt == 's';
  pto.LoopVectorization = vec;
  pto.SLPVectorization = vec || opt == 'z';
  pto.LoopUnrolling = opt >= 2 || opt == 's';
  pto.LoopInterleaving = pto.LoopUnrolling;
  return pto;
}

// -O<level>: 0..3, s or z.
char level_name(int opt) { return opt <= 3 ? '0' + opt : opt; }

// After a module's last optimization: inline `rok.alloc`, then above -O0
// clean up. Not at -O0, where InstCombine would delete store-only slots and
// with them `-g`'s view of unused parameters.
void inline_alloc(llvm::Module &m, llvm::TargetMachine &tm, int opt) {
  if (!m.getFunction(ALLOC))
    return;
  llvm::LoopAnalysisManager lam;
  llvm::FunctionAnalysisManager fam;
  llvm::CGSCCAnalysisManager cgam;
  llvm::ModuleAnalysisManager mam;
  llvm::PassBuilder pb(&tm, tuning(opt));
  pb.registerModuleAnalyses(mam);
  pb.registerCGSCCAnalyses(cgam);
  pb.registerFunctionAnalyses(fam);
  pb.registerLoopAnalyses(lam);
  pb.crossRegisterProxies(lam, fam, cgam, mam);
  llvm::ModulePassManager mpm;
  mpm.addPass(InlineAlloc());
  mpm.addPass(llvm::AlwaysInlinerPass());
  if (opt != 0) {
    llvm::FunctionPassManager fpm;
    fpm.addPass(llvm::InstCombinePass());
    fpm.addPass(llvm::SimplifyCFGPass());
    fpm.addPass(llvm::InstCombinePass());
    mpm.addPass(llvm::createModuleToFunctionPassAdaptor(std::move(fpm)));
  }
  mpm.run(m, mam);
}

// Non-trivial loop unswitching (LLVM: -O3 only) at -O2 too: a bounds check
// of an invariant index after a store leaves the loop only by versioning.
// The switch is global (thin backends take no callbacks), so each
// optimization sets it; a concurrent one at another level may see it,
// which changes how much it unswitches, never what its code does.
void unswitch_nontrivial(int opt) {
  static auto *on = static_cast<llvm::cl::opt<bool> *>(
      llvm::cl::getRegisteredOptions().lookup("enable-nontrivial-unswitch"));
  if (!on)
    llvm::report_fatal_error("LLVM has no -enable-nontrivial-unswitch");
  on->setValue(opt == 2);
}

// `m` through clang -O<n>'s pipeline, then `inline_alloc`; with `thin`, the
// ThinLTO pre-link pipeline instead, written as bitcode with a summary.
void optimize(llvm::Module &m, llvm::TargetMachine &tm, const RokCompile &o,
              llvm::raw_ostream *thin) {
  // As clang marks every function at -Os: the inliner and the code
  // generator read the attribute, not the pipeline's level.
  if (o.opt == 's')
    for (llvm::Function &f : m)
      if (!f.isDeclaration())
        f.addFnAttr(llvm::Attribute::OptimizeForSize);
  if (llvm::Function *f = m.getFunction(ALLOC)) {
    f->addFnAttr(llvm::Attribute::get(
        m.getContext(), llvm::Attribute::AllocKind,
        uint64_t(llvm::AllocFnKind::Alloc | llvm::AllocFnKind::Zeroed)));
    f->addRetAttr(llvm::Attribute::NoAlias);
  }
  llvm::LoopAnalysisManager lam;
  llvm::FunctionAnalysisManager fam;
  llvm::CGSCCAnalysisManager cgam;
  llvm::ModuleAnalysisManager mam;
  std::optional<llvm::PGOOptions> pgo;
  bool generate = *o.profile_generate, use = *o.profile_use;
  if (generate && use)
    // Context-sensitive: optimized with the first stage's profile, then
    // instrumented again after inlining.
    pgo.emplace(o.profile_use, o.profile_generate, "", "",
                llvm::PGOOptions::IRUse, llvm::PGOOptions::CSIRInstr);
  else if (generate)
    pgo.emplace(o.profile_generate, "", "", "", llvm::PGOOptions::IRInstr);
  else if (use)
    pgo.emplace(o.profile_use, "", "", "", llvm::PGOOptions::IRUse,
                has_cs_profile(o.profile_use) ? llvm::PGOOptions::CSIRUse
                                              : llvm::PGOOptions::NoCSAction);
  unswitch_nontrivial(o.opt);
  llvm::PassBuilder pb(&tm, tuning(o.opt), pgo);
  pb.registerModuleAnalyses(mam);
  pb.registerCGSCCAnalyses(cgam);
  pb.registerFunctionAnalyses(fam);
  pb.registerLoopAnalyses(lam);
  pb.crossRegisterProxies(lam, fam, cgam, mam);
  llvm::ModulePassManager mpm;
  std::string level = std::string("O") + level_name(o.opt);
  std::string pipeline = o.opt == 0 ? "default<O0>"
                         : thin     ? "thinlto-pre-link<" + level + ">"
                                    : "default<" + level + ">";
  if (auto e = pb.parsePassPipeline(mpm, pipeline))
    llvm::report_fatal_error(std::move(e));
  mpm.run(m, mam);
  if (thin)
    llvm::WriteBitcodeToFile(
        m, *thin, false, &mam.getResult<llvm::ModuleSummaryIndexAnalysis>(m),
        true);
  else
    inline_alloc(m, tm, o.opt);
}

int codegen(llvm::Module &m, llvm::TargetMachine &tm,
            llvm::CodeGenFileType kind, RokBuf *out, RokBuf *err) {
  llvm::SmallString<0> buf;
  llvm::raw_svector_ostream os(buf);
  llvm::legacy::PassManager cg;
  if (tm.addPassesToEmitFile(cg, os, nullptr, kind))
    return fail(err, "the target cannot emit this file type");
  cg.run(m);
  put(out, buf.str());
  return 0;
}

// LLVM diagnostics while optimizing: errors fail, warnings go back to rokc;
// functions whose control flow changed since profiling are only counted.
struct Diags {
  std::string errors, warnings;
  unsigned stale = 0;
  std::mutex lock;
};

void on_diag(const llvm::DiagnosticInfo *d, void *ctx) {
  auto *s = static_cast<Diags *>(ctx);
  auto sev = d->getSeverity();
  if (sev != llvm::DS_Error && sev != llvm::DS_Warning)
    return;
  std::string msg;
  llvm::raw_string_ostream os(msg);
  std::lock_guard<std::mutex> guard(s->lock);
  if (auto *p = llvm::dyn_cast<llvm::DiagnosticInfoPGOProfile>(d)) {
    if (sev == llvm::DS_Warning && p->getMsg().str().find("hash mismatch") !=
                                       std::string::npos) {
      ++s->stale;
      return;
    }
    os << p->getMsg();
  } else {
    llvm::DiagnosticPrinterRawOStream printer(os);
    d->print(printer);
  }
  (sev == llvm::DS_Error ? s->errors : s->warnings) += msg + "\n";
}

// Pure data: a copy is as good as the original.
bool copyable(const llvm::GlobalValue *g) {
  auto *v = llvm::dyn_cast<llvm::GlobalVariable>(g);
  return v && v->hasLocalLinkage() && v->isConstant() &&
         v->hasGlobalUnnamedAddr() && !v->isThreadLocal() &&
         llvm::isa<llvm::ConstantData>(v->getInitializer());
}

// Cut `m` into `n` similar-sized bitcode modules (one context can't compile
// in parallel). As llvm::SplitModule, locals stay with their users, but a
// pure-data local constant (a literal) is copied into each user part, or
// shared literals would glue most of the module together.
std::vector<llvm::SmallString<0>> split(llvm::Module &m, size_t n) {
  using GV = const llvm::GlobalValue *;
  llvm::EquivalenceClasses<GV> together;
  llvm::DenseMap<const llvm::Comdat *, GV> comdats;
  llvm::DenseMap<GV, size_t> order;
  // Everything `g` is used by, through constant expressions.
  auto users = [&](GV g, const llvm::Value *v) {
    llvm::SmallVector<const llvm::User *, 8> work(v->user_begin(),
                                                  v->user_end());
    while (!work.empty()) {
      const llvm::User *u = work.pop_back_val();
      if (auto *i = llvm::dyn_cast<llvm::Instruction>(u))
        together.unionSets(g, i->getFunction());
      else if (auto *gu = llvm::dyn_cast<llvm::GlobalValue>(u))
        together.unionSets(g, gu);
      else
        work.append(u->user_begin(), u->user_end());
    }
  };
  auto record = [&](llvm::GlobalValue &g) {
    if (g.isDeclaration() || copyable(&g))
      return;
    order[&g] = order.size();
    together.insert(&g);
    if (const llvm::Comdat *c = g.getComdat()) {
      auto [it, fresh] = comdats.try_emplace(c, &g);
      if (!fresh)
        together.unionSets(it->second, &g);
    }
    if (const llvm::GlobalObject *root = g.getAliaseeObject()) {
      if (auto *ifunc = llvm::dyn_cast<llvm::GlobalIFunc>(root))
        root = ifunc->getResolverFunction();
      if (root != &g)
        together.unionSets(&g, root);
    }
    if (auto *f = llvm::dyn_cast<llvm::Function>(&g))
      for (const llvm::BasicBlock &bb : *f)
        if (auto *ba = llvm::BlockAddress::lookup(&bb))
          users(f, ba);
    if (g.hasLocalLinkage())
      users(&g, &g);
  };
  for (auto &g : m.global_values())
    record(g);

  // Largest cluster first, onto the lightest part.
  struct Cluster {
    GV leader;
    size_t first, size;
  };
  std::vector<Cluster> clusters;
  for (const auto &c : together) {
    if (!c->isLeader())
      continue;
    Cluster k{c->getData(), SIZE_MAX, 0};
    for (auto i = together.member_begin(*c); i != together.member_end(); ++i) {
      k.first = std::min(k.first, order[*i]);
      auto *f = llvm::dyn_cast<llvm::Function>(*i);
      k.size += f ? f->getInstructionCount() + 1 : 1;
    }
    clusters.push_back(k);
  }
  std::sort(clusters.begin(), clusters.end(), [](auto &a, auto &b) {
    return a.size != b.size ? a.size > b.size : a.first < b.first;
  });
  std::vector<size_t> load(n);
  llvm::DenseMap<GV, size_t> part_of;
  for (const Cluster &k : clusters) {
    size_t p = std::min_element(load.begin(), load.end()) - load.begin();
    load[p] += k.size;
    for (auto i = together.findLeader(k.leader); i != together.member_end();
         ++i)
      part_of[*i] = p;
  }

  std::vector<llvm::SmallString<0>> parts(n);
  for (size_t p = 0; p < n; ++p) {
    llvm::ValueToValueMapTy map;
    std::unique_ptr<llvm::Module> part =
        llvm::CloneModule(m, map, [&](GV g) {
          auto it = part_of.find(g);
          return it != part_of.end() ? it->second == p : copyable(g);
        });
    if (p)
      part->setModuleInlineAsm("");
    for (llvm::GlobalVariable &v : llvm::make_early_inc_range(part->globals()))
      if (copyable(&v)) {
        v.removeDeadConstantUsers();
        if (v.use_empty())
          v.eraseFromParent();
      }
    llvm::raw_svector_ostream os(parts[p]);
    llvm::WriteBitcodeToFile(*part, os);
  }
  return parts;
}

// Runs `job(i)` for i < n on up to a thread per core.
template <typename F> void in_parallel(size_t n, F job) {
  std::atomic<size_t> next{0};
  auto work = [&] {
    for (size_t i; (i = next++) < n;)
      job(i);
  };
  size_t k = std::min<size_t>(n, std::max(1u, std::thread::hardware_concurrency()));
  std::vector<std::thread> threads;
  for (size_t t = 0; t < k; ++t)
    threads.emplace_back(work);
  for (auto &t : threads)
    t.join();
}

// Errors of `d` into `err`; warnings into `warn`.
int report(Diags &d, RokBuf *err, RokBuf *warn) {
  if (!d.errors.empty())
    return fail(err, d.errors);
  if (d.stale)
    d.warnings += "the profile is out of date for " + std::to_string(d.stale) +
                  " function(s) whose control flow changed since it was "
                  "recorded; they are optimized without it\n";
  if (!d.warnings.empty())
    put(warn, d.warnings);
  return 0;
}

// `rok.alloc`'s body out of `m` (left a declaration), as a module of its
// own in bitcode; empty when `m` has none.
llvm::SmallString<0> take_alloc(llvm::Module &m) {
  llvm::SmallString<0> alloc;
  if (llvm::Function *f = m.getFunction(ALLOC); f && !f->isDeclaration()) {
    llvm::ValueToValueMapTy map;
    auto body = llvm::CloneModule(m, map, [](const llvm::GlobalValue *g) {
      return g->getName() == ALLOC;
    });
    body->getFunction(ALLOC)->setLinkage(llvm::GlobalValue::ExternalLinkage);
    llvm::raw_svector_ostream os(alloc);
    llvm::WriteBitcodeToFile(*body, os);
    f->deleteBody();
  }
  return alloc;
}

// Thin link of `parts` (bitcode with summaries, ids `names`) into objects:
// import what inlining wants across parts, then optimize and compile in
// parallel. Symbols stay visible unless `internal` says otherwise. After
// each backend pipeline `alloc` is linked in and inlined.
//
// With `cache`, a part with unchanged inputs reuses its object (`hits`);
// unused entries are deleted.
int thin_link(const std::vector<llvm::SmallString<0>> &parts,
              const std::vector<std::string> &names, llvm::StringRef alloc,
              const std::function<bool(llvm::StringRef)> &internal,
              const RokCompile &o, const char *cache, size_t *hits,
              std::vector<llvm::SmallString<0>> &out, Diags &diags,
              RokBuf *err) {
  size_t n = parts.size();
  llvm::Triple triple(o.triple);
  llvm::lto::Config conf;
  conf.CPU = o.cpu;
  for (llvm::StringRef f = o.features; !f.empty();) {
    auto [first, rest] = f.split(',');
    conf.MAttrs.push_back(first.str());
    f = rest;
  }
  conf.Options = target_options(triple);
  conf.CGOptLevel = codegen_level(o.opt);
  conf.OptPipeline = std::string("thinlto<O") + level_name(o.opt) + ">";
  unswitch_nontrivial(o.opt);
  conf.PTO = tuning(o.opt);
  conf.DisableVerify = true;
  conf.DiagHandler = [&](const llvm::DiagnosticInfo &d) {
    on_diag(&d, &diags);
  };
  // Every module is a ThinLTO part: no regular LTO object.
  conf.PreOptModuleHook = [](unsigned task, const llvm::Module &) {
    return task != 0;
  };
  // The hook is handed the module it may still change before codegen.
  conf.PostOptModuleHook = [&](unsigned, const llvm::Module &cpart) {
    auto &part = const_cast<llvm::Module &>(cpart);
    llvm::Function *f = part.getFunction(ALLOC);
    if (!f || alloc.empty())
      return true;
    auto body = llvm::parseBitcodeFile(llvm::MemoryBufferRef(alloc, ALLOC),
                                       part.getContext());
    std::string error;
    auto tm = target_machine(o, error);
    if (!body || !tm ||
        llvm::Linker::linkModules(part, std::move(*body),
                                  llvm::Linker::LinkOnlyNeeded)) {
      std::lock_guard<std::mutex> guard(diags.lock);
      diags.errors += body ? error.empty() ? "cannot link `rok.alloc`\n"
                                           : error + "\n"
                           : llvm::toString(body.takeError()) + "\n";
      return false;
    }
    part.getFunction(ALLOC)->setLinkage(llvm::GlobalValue::InternalLinkage);
    inline_alloc(part, *tm, o.opt);
    return true;
  };
  llvm::lto::LTO lto(std::move(conf),
                     llvm::lto::createInProcessThinBackend(
                         llvm::heavyweight_hardware_concurrency(n)));
  for (size_t i = 0; i < n; ++i) {
    auto input = llvm::lto::InputFile::create(
        llvm::MemoryBufferRef(parts[i], names[i]));
    if (!input)
      return fail(err, llvm::toString(input.takeError()));
    std::vector<llvm::lto::SymbolResolution> res;
    for (const auto &sym : (*input)->symbols()) {
      llvm::lto::SymbolResolution r;
      r.Prevailing = !sym.isUndefined();
      r.VisibleToRegularObj = r.Prevailing && !internal(sym.getIRName());
      res.push_back(r);
    }
    if (auto e = lto.add(std::move(*input), res))
      return fail(err, llvm::toString(std::move(e)));
  }
  // Task 0 is the regular LTO object; part i is task i + 1.
  std::vector<llvm::SmallString<0>> objects(lto.getMaxTasks());
  auto add = [&](unsigned task, const llvm::Twine &)
      -> llvm::Expected<std::unique_ptr<llvm::CachedFileStream>> {
    return std::make_unique<llvm::CachedFileStream>(
        std::make_unique<llvm::raw_svector_ostream>(objects[task]));
  };
  if (!*cache) {
    if (auto e = lto.run(add))
      return fail(err, llvm::toString(std::move(e)));
  } else {
    auto local = llvm::localCache(
        "ThinLTO", "rok-thin", cache,
        [&](unsigned task, const llvm::Twine &,
            std::unique_ptr<llvm::MemoryBuffer> mb) {
          objects[task] = mb->getBuffer();
        });
    if (!local)
      return fail(err, llvm::toString(local.takeError()));
    std::mutex lock;
    llvm::StringSet<> used;
    llvm::FileCache counted(
        [&](unsigned task, llvm::StringRef key, const llvm::Twine &name)
            -> llvm::Expected<llvm::AddStreamFn> {
          {
            std::lock_guard<std::mutex> guard(lock);
            used.insert("llvmcache-" + key.str());
          }
          auto stream = (*local)(task, key, name);
          if (stream && !*stream) {
            std::lock_guard<std::mutex> guard(lock);
            ++*hits;
          }
          return stream;
        },
        local->getCacheDirectoryPath());
    if (auto e = lto.run(add, counted))
      return fail(err, llvm::toString(std::move(e)));
    std::error_code ec;
    for (llvm::sys::fs::directory_iterator it(cache, ec), end; it != end && !ec;
         it.increment(ec)) {
      llvm::StringRef file = llvm::sys::path::filename(it->path());
      if (file.starts_with("llvmcache-") && !used.contains(file))
        llvm::sys::fs::remove(it->path());
    }
  }
  for (size_t i = 0; i < n; ++i)
    out.push_back(std::move(objects[i + 1]));
  return 0;
}

// An optimized object in `o.parts` parts via in-process ThinLTO: pre-link
// per part on its own thread, then `thin_link`. Linkage as a whole-module
// compile: unreferenced linkonce definitions go, the rest stays visible.
// Parts see `rok.alloc` only as a declaration (its attributes suffice for
// escape elision, and imported code calls the same one).
int thin(llvm::Module &m, const RokCompile &o, RokBuf *out, RokBuf *err,
         RokBuf *warn) {
  llvm::SmallString<0> alloc = take_alloc(m);
  llvm::StringSet<> droppable;
  for (llvm::GlobalValue &g : m.global_values())
    if (g.hasLinkOnceLinkage())
      droppable.insert(g.getName());
  std::vector<llvm::SmallString<0>> parts = split(m, o.parts);
  size_t n = parts.size();
  Diags diags;
  std::vector<std::string> errors(n);
  in_parallel(n, [&](size_t i) {
    llvm::LLVMContext ctx;
    ctx.setDiagnosticHandlerCallBack(on_diag, &diags);
    auto part =
        llvm::parseBitcodeFile(llvm::MemoryBufferRef(parts[i], "part"), ctx);
    if (!part) {
      errors[i] = llvm::toString(part.takeError());
      return;
    }
    auto tm = target_machine(o, errors[i]);
    if (!tm)
      return;
    llvm::SmallString<0> summary;
    llvm::raw_svector_ostream os(summary);
    optimize(**part, *tm, o, &os);
    parts[i] = std::move(summary);
  });
  for (auto &e : errors)
    if (!e.empty())
      return fail(err, e);
  if (!diags.errors.empty())
    return fail(err, diags.errors);
  std::vector<std::string> names(n);
  for (size_t i = 0; i < n; ++i)
    names[i] = "part" + std::to_string(i);
  std::vector<llvm::SmallString<0>> objects;
  size_t hits = 0;
  if (thin_link(
          parts, names, alloc,
          [&](llvm::StringRef s) { return droppable.contains(s); }, o, "",
          &hits, objects, diags, err))
    return 1;
  if (report(diags, err, warn))
    return 1;
  for (size_t i = 0; i < n; ++i)
    put(&out[i], objects[i].str());
  return 0;
}

void tls_model(llvm::Module &m, const RokCompile &o) {
  if (o.tls_initial_exec) {
    for (llvm::GlobalVariable &g : m.globals())
      if (g.isThreadLocal() && g.getName().starts_with("rok_tls"))
        g.setThreadLocalMode(llvm::GlobalValue::InitialExecTLSModel);
  }
}

// The module for the requested target (as clang -target overrides the
// triple of an IR input), verified; its target machine.
std::unique_ptr<llvm::TargetMachine> prepare(llvm::Module &m,
                                             const RokCompile &o,
                                             RokBuf *err) {
  tls_model(m, o);
  std::string error;
  auto tm = target_machine(o, error);
  if (!tm) {
    fail(err, error);
    return nullptr;
  }
  m.setTargetTriple(tm->getTargetTriple());
  m.setDataLayout(tm->createDataLayout());
  // As clang: each function carries its CPU and features so the inliner
  // and vectorizers see them.
  for (llvm::Function &f : m)
    if (!f.isDeclaration()) {
      f.addFnAttr("target-cpu", tm->getTargetCPU());
      if (!tm->getTargetFeatureString().empty())
        f.addFnAttr("target-features", tm->getTargetFeatureString());
    }
  if (tm->getTargetTriple().isOSWindows()) {
    // Windows debuggers read CodeView (lld-link writes it into a PDB).
    if (m.getModuleFlag("Debug Info Version") && !m.getModuleFlag("CodeView"))
      m.addModuleFlag(llvm::Module::Warning, "CodeView", 1);
    // Every function in .pdata / .xdata: without an entry the unwinder
    // takes a function for a leaf and cannot walk past it.
    for (llvm::Function &f : m)
      if (!f.isDeclaration())
        f.setUWTableKind(llvm::UWTableKind::Async);
    // COFF merges duplicates only via COMDATs: as clang, each linkonce /
    // weak definition gets its own, or a second definition elsewhere is a
    // duplicate symbol.
    for (llvm::GlobalObject &g : m.global_objects())
      if (!g.isDeclaration() && !g.hasComdat() &&
          (g.hasLinkOnceLinkage() || g.hasWeakLinkage()))
        g.setComdat(m.getOrInsertComdat(g.getName()));
  }
  std::string msg;
  llvm::raw_string_ostream os(msg);
  if (llvm::verifyModule(m, &os)) {
    fail(err, "invalid LLVM IR:\n" + msg);
    return nullptr;
  }
  return tm;
}

// The module in the requested output; LLVM's warnings in `warn`.
int finish(llvm::Module &m, const RokCompile &o, RokBuf *out, RokBuf *err,
           RokBuf *warn) {
  if (o.emit == 0) {
    tls_model(m, o);
    std::string text;
    llvm::raw_string_ostream os(text);
    m.print(os, nullptr);
    put(out, text);
    return 0;
  }
  auto tm = prepare(m, o, err);
  if (!tm)
    return 1;
  bool parts = o.parts > 1 && o.emit == 2;
  // IR PGO instruments and reads profiles in the whole-module pipeline:
  // a profile build optimizes whole and splits only code generation.
  if (parts && o.opt != 0 && !*o.profile_generate && !*o.profile_use)
    return thin(m, o, out, err, warn);
  Diags diags;
  m.getContext().setDiagnosticHandlerCallBack(on_diag, &diags);
  optimize(m, *tm, o, nullptr);
  if (report(diags, err, warn))
    return 1;
  auto kind = o.emit == 1 ? llvm::CodeGenFileType::AssemblyFile
                          : llvm::CodeGenFileType::ObjectFile;
  if (!parts)
    return codegen(m, *tm, kind, out, err);
  // Optimized whole (-O0 or a profile build): each part is compiled on
  // a thread.
  std::vector<llvm::SmallString<0>> bitcode = split(m, o.parts);
  std::vector<int> rc(bitcode.size());
  in_parallel(bitcode.size(), [&](size_t i) {
    llvm::LLVMContext ctx;
    auto part = llvm::parseBitcodeFile(
        llvm::MemoryBufferRef(bitcode[i].str(), "part"), ctx);
    if (!part) {
      rc[i] = fail(&out[i], llvm::toString(part.takeError()));
      return;
    }
    std::string error;
    auto ptm = target_machine(o, error);
    rc[i] = ptm ? codegen(**part, *ptm, kind, &out[i], &out[i])
                : fail(&out[i], error);
  });
  for (size_t i = 0; i < rc.size(); ++i)
    if (rc[i]) {
      put(err, llvm::StringRef(out[i].ptr, out[i].len));
      return 1;
    }
  return 0;
}

// MLIR's di_composite_type has no discriminator and member extraData is a
// node, so rokc writes a variant part as the discriminant member then one
// member per variant whose di_subrange count is its tag; this turns them
// into DWARF variants.
void variant_parts(llvm::Module &m) {
  llvm::DebugInfoFinder finder;
  finder.processModule(m);
  llvm::DIBuilder b(m, false);
  auto *i32 = llvm::Type::getInt32Ty(m.getContext());
  std::vector<llvm::DICompositeType *> parents;
  for (llvm::DIType *t : finder.types())
    if (auto *p = llvm::dyn_cast<llvm::DICompositeType>(t))
      parents.push_back(p);
  for (llvm::DICompositeType *p : parents) {
    auto *els = llvm::dyn_cast_or_null<llvm::MDTuple>(p->getRawElements());
    for (unsigned i = 0; els && i < els->getNumOperands(); ++i) {
      auto *vp = llvm::dyn_cast_or_null<llvm::DICompositeType>(els->getOperand(i));
      if (!vp || vp->getTag() != llvm::dwarf::DW_TAG_variant_part ||
          vp->getDiscriminator() || vp->getElements().empty())
        continue;
      auto parts = vp->getElements();
      auto *d = llvm::cast<llvm::DIDerivedType>(parts[0]);
      auto *discr = b.createMemberType(p, "", nullptr, 0, d->getSizeInBits(),
                                       0, d->getOffsetInBits(),
                                       llvm::DINode::FlagArtificial,
                                       d->getBaseType());
      std::vector<llvm::Metadata *> variants;
      for (unsigned k = 1; k < parts.size(); ++k) {
        auto *v = llvm::cast<llvm::DIDerivedType>(parts[k]);
        auto *range = llvm::cast<llvm::DISubrange>(v->getExtraData());
        auto *tag = llvm::cast<llvm::ConstantInt *>(range->getCount());
        variants.push_back(b.createVariantMemberType(
            p, v->getName(), nullptr, 0, v->getSizeInBits(), 0,
            v->getOffsetInBits(),
            llvm::ConstantInt::get(i32, tag->getZExtValue()),
            llvm::DINode::FlagZero, v->getBaseType()));
      }
      els->replaceOperandWith(
          i, b.createVariantPart(p, "", nullptr, 0, 0, 0,
                                 llvm::DINode::FlagZero, discr,
                                 b.getOrCreateArray(variants)));
    }
  }
}

// The MLIR module `src` (standard dialects) through `o.mlir_pipeline`
// into LLVM IR in `llctx`; null and the reason in `err` if it fails.
std::unique_ptr<llvm::Module> translate(llvm::StringRef src,
                                        const RokCompile &o,
                                        llvm::LLVMContext &llctx,
                                        RokBuf *err) {
  mlir::DialectRegistry registry;
  register_dialects(registry);
  init_passes();
  // Nested passes run on one pool shared by every compile in the process,
  // not a pool per context.
  static llvm::DefaultThreadPool threads;
  mlir::MLIRContext ctx(registry, mlir::MLIRContext::Threading::DISABLED);
  ctx.setThreadPool(threads);

  std::string diag;
  llvm::raw_string_ostream diag_os(diag);
  llvm::SourceMgr sm;
  sm.AddNewSourceBuffer(llvm::MemoryBuffer::getMemBufferCopy(src, "<stdin>"),
                        llvm::SMLoc());
  mlir::SourceMgrDiagnosticHandler handler(sm, &ctx, diag_os);

  mlir::OwningOpRef<mlir::ModuleOp> module =
      mlir::parseSourceFile<mlir::ModuleOp>(sm, &ctx);
  mlir::PassManager pm(&ctx, mlir::ModuleOp::getOperationName());
  if (!module || mlir::failed(mlir::parsePassPipeline(o.mlir_pipeline, pm, diag_os)) ||
      mlir::failed(pm.run(*module))) {
    fail(err, diag);
    return nullptr;
  }
  // Verified later, after `variant_parts`.
  auto m = mlir::translateModuleToLLVMIR(*module, llctx, "LLVMDialectModule",
                                         /*disableVerification=*/true);
  if (!m) {
    fail(err, diag.empty() ? "cannot translate MLIR to LLVM IR" : diag);
    return nullptr;
  }
  variant_parts(*m);
  return m;
}

} // namespace

extern "C" {

// The MLIR module `src` through `translate`, then `finish`.
int rok_llvm_compile_mlir(const char *src, size_t len, const RokCompile *o,
                          RokBuf *out, RokBuf *err, RokBuf *warn) {
  llvm::LLVMContext llctx;
  auto m = translate(llvm::StringRef(src, len), *o, llctx, err);
  if (!m)
    return 1;
  return finish(*m, *o, out, err, warn);
}

// One codegen unit of a program: MLIR text to compile, or (`bitcode`) its
// ThinLTO bitcode from an earlier build.
struct RokUnit {
  const char *name;
  const char *src;
  size_t len;
  int bitcode;
};

// An optimized object per unit via in-process ThinLTO: each unit through
// `translate` and pre-link in parallel (bitcode to `bitcode[i]` for next
// time), then `thin_link`, reusing `cache`. `alloc` defines `rok.alloc`,
// which units only declare. A program's `_Y` symbols may be internalized;
// a `library`'s only `.copy_Y` and `_gctype.` ones. `hits` counts cache
// hits.
int rok_llvm_compile_units(const RokUnit *units, size_t n, const char *alloc,
                           size_t alloc_len, const RokCompile *o,
                           const char *cache, int library, RokBuf *objects,
                           RokBuf *bitcode, size_t *hits, RokBuf *err,
                           RokBuf *warn) {
  llvm::SmallString<0> body;
  {
    llvm::LLVMContext ctx;
    auto m = translate(llvm::StringRef(alloc, alloc_len), *o, ctx, err);
    if (!m || !prepare(*m, *o, err))
      return 1;
    body = take_alloc(*m);
  }
  std::vector<llvm::SmallString<0>> parts(n);
  std::vector<std::string> names(n), errors(n);
  Diags diags;
  in_parallel(n, [&](size_t i) {
    names[i] = units[i].name;
    if (units[i].bitcode) {
      parts[i] = llvm::StringRef(units[i].src, units[i].len);
      return;
    }
    llvm::LLVMContext ctx;
    ctx.setDiagnosticHandlerCallBack(on_diag, &diags);
    RokBuf e{nullptr, 0};
    auto m = translate(llvm::StringRef(units[i].src, units[i].len), *o, ctx, &e);
    auto tm = m ? prepare(*m, *o, &e) : nullptr;
    if (!tm) {
      errors[i] = std::string(e.ptr, e.len);
      rok_llvm_free(&e);
      return;
    }
    take_alloc(*m);
    // The name feeds the module hash that promoted locals carry, so the same
    // code in two packages can't promote to one symbol.
    m->setSourceFileName(units[i].name);
    llvm::raw_svector_ostream os(parts[i]);
    optimize(*m, *tm, *o, &os);
  });
  for (auto &e : errors)
    if (!e.empty())
      return fail(err, e);
  if (!diags.errors.empty())
    return fail(err, diags.errors);
  for (size_t i = 0; i < n; ++i)
    if (!units[i].bitcode)
      put(&bitcode[i], parts[i].str());
  std::vector<llvm::SmallString<0>> out;
  if (thin_link(
          parts, names, body,
          [&](llvm::StringRef s) {
            return library ? s.contains(".copy_Y") || s.contains("_gctype.")
                           : s.starts_with("_Y");
          },
          *o, cache, hits, out, diags, err))
    return 1;
  if (report(diags, err, warn))
    return 1;
  for (size_t i = 0; i < n; ++i)
    put(&objects[i], out[i].str());
  return 0;
}

// LLVM IR text `src` through `finish`.
int rok_llvm_compile_ir(const char *src, size_t len, const RokCompile *o,
                        RokBuf *out, RokBuf *err, RokBuf *warn) {
  llvm::LLVMContext ctx;
  llvm::SMDiagnostic d;
  // The IR lexer reads up to a NUL.
  auto buf = llvm::MemoryBuffer::getMemBufferCopy(llvm::StringRef(src, len),
                                                  "<stdin>");
  auto m = llvm::parseIR(buf->getMemBufferRef(), d, ctx);
  if (!m) {
    std::string msg;
    llvm::raw_string_ostream os(msg);
    d.print("rokc", os, false);
    return fail(err, msg);
  }
  return finish(*m, *o, out, err, warn);
}

// The build machine's CPU and its features (`+f,-g,..`), for
// `-C target-cpu=native`; 1 if LLVM cannot name the CPU.
int rok_llvm_host_cpu(RokBuf *cpu, RokBuf *features) {
  llvm::StringRef name = llvm::sys::getHostCPUName();
  if (name.empty())
    return 1;
  put(cpu, name);
  std::string f;
  for (auto &kv : llvm::sys::getHostCPUFeatures()) {
    if (!f.empty())
      f += ',';
    f += kv.second ? '+' : '-';
    f += kv.first().str();
  }
  put(features, f);
  return 0;
}

// The data layout LLVM uses for o->triple / o->cpu.
int rok_llvm_data_layout(const RokCompile *o, RokBuf *out, RokBuf *err) {
  std::string error;
  auto tm = target_machine(*o, error);
  if (!tm)
    return fail(err, error);
  put(out, tm->createDataLayout().getStringRepresentation());
  return 0;
}

// The triple LLVM was configured to run on.
// dsymutil's command line (argv[0] is "dsymutil"); it reports on stderr.
int rok_llvm_dsymutil(const char *const *argv, size_t argc) {
  static std::mutex lock;
  std::lock_guard<std::mutex> guard(lock);
  std::vector<char *> args;
  for (size_t i = 0; i < argc; ++i)
    args.push_back(const_cast<char *>(argv[i]));
  args.push_back(nullptr);
  llvm::ToolContext ctx{};
  return dsymutil_main(static_cast<int>(argc), args.data(), ctx);
}

// lld with argv[0] naming the flavor (ld64.lld, ld.lld, lld-link, wasm-ld;
// never MinGW's); output to `log`; returns its exit code.
int rok_llvm_lld(const char *const *argv, size_t argc, RokBuf *log) {
  static std::mutex lock; // lld keeps per-link globals
  std::lock_guard<std::mutex> guard(lock);
  std::string text;
  llvm::raw_string_ostream os(text);
  lld::Result r = lld::lldMain(llvm::ArrayRef<const char *>(argv, argc), os,
                               os,
                               {{lld::WinLink, &lld::coff::link},
                                {lld::Gnu, &lld::elf::link},
                                {lld::Darwin, &lld::macho::link},
                                {lld::Wasm, &lld::wasm::link}});
  put(log, text);
  return r.retCode;
}

// An archive of the files `members` with a symbol table; `kind` 0 GNU,
// 1 BSD (Darwin), 2 COFF.
int rok_llvm_write_archive(const char *path, const char *const *members,
                           size_t n, int kind, RokBuf *err) {
  init_llvm();
  std::vector<llvm::NewArchiveMember> ms;
  for (size_t i = 0; i < n; ++i) {
    auto m = llvm::NewArchiveMember::getFile(members[i], true);
    if (!m)
      return fail(err, llvm::toString(m.takeError()));
    // Named by the file alone, as ar names it.
    m->MemberName = llvm::sys::path::filename(members[i]);
    ms.push_back(std::move(*m));
  }
  auto format = kind == 1   ? llvm::object::Archive::K_DARWIN
                : kind == 2 ? llvm::object::Archive::K_COFF
                            : llvm::object::Archive::K_GNU;
  if (auto e = llvm::writeArchive(path, ms, llvm::SymtabWritingMode::NormalSymtab,
                                  format, true, false))
    return fail(err, llvm::toString(std::move(e)));
  return 0;
}

// Merge raw or indexed profiles into indexed `path` (llvm-profdata merge),
// skipping empty raw ones; per-record problems are warnings, as there.
int rok_llvm_profdata_merge(const char *const *inputs, size_t n,
                            const char *path, RokBuf *warn, RokBuf *err) {
  auto fs = llvm::vfs::getRealFileSystem();
  llvm::InstrProfWriter writer;
  std::string errors, warnings;
  auto report = [&](llvm::Error e, llvm::StringRef file) {
    errors += file.str() + ": " + llvm::toString(std::move(e)) + "\n";
  };
  for (size_t i = 0; i < n; ++i) {
    auto reader = llvm::InstrProfReader::create(inputs[i], *fs);
    if (!reader) {
      auto [code, msg] = llvm::InstrProfError::take(reader.takeError());
      if (code != llvm::instrprof_error::empty_raw_profile)
        report(llvm::make_error<llvm::InstrProfError>(code, msg), inputs[i]);
      continue;
    }
    if (auto e = writer.mergeProfileKind((*reader)->getProfileKind())) {
      report(std::move(e), inputs[i]);
      continue;
    }
    for (auto &r : **reader) {
      std::string name = r.Name.str();
      writer.addRecord(std::move(r), 1, [&](llvm::Error e) {
        warnings += std::string(inputs[i]) + ": " + name + ": " +
                    llvm::toString(std::move(e)) + "\n";
      });
    }
    if ((*reader)->hasError())
      if (auto e = (*reader)->getError())
        report(std::move(e), inputs[i]);
  }
  if (!errors.empty())
    return fail(err, errors);
  std::error_code ec;
  llvm::raw_fd_ostream os(path, ec, llvm::sys::fs::OF_None);
  if (ec)
    return fail(err, std::string(path) + ": " + ec.message());
  if (auto e = writer.write(os))
    return fail(err, llvm::toString(std::move(e)));
  put(warn, warnings);
  return 0;
}

// Summary of profile `path` (llvm-profdata show): `all` lists each
// function's hash and counter count, `topn` the hottest; context-sensitive
// records are summed apart.
int rok_llvm_profdata_show(const char *path, int all, size_t topn,
                           RokBuf *out, RokBuf *err) {
  auto fs = llvm::vfs::getRealFileSystem();
  auto reader = llvm::InstrProfReader::create(path, *fs);
  if (!reader)
    return fail(err, std::string(path) + ": " +
                         llvm::toString(reader.takeError()));
  auto &r = **reader;
  std::string text;
  llvm::raw_string_ostream os(text);
  llvm::InstrProfSummaryBuilder builder(llvm::ProfileSummaryBuilder::DefaultCutoffs);
  std::vector<std::pair<uint64_t, std::string>> top;
  size_t shown = 0, cs = 0;
  for (const auto &f : r) {
    if (llvm::NamedInstrProfRecord::hasCSFlagInHash(f.Hash)) {
      ++cs;
      continue;
    }
    builder.addRecord(f);
    uint64_t max = 0;
    for (uint64_t c : f.Counts)
      if (c != uint64_t(-1))
        max = std::max(max, c);
    if (topn) {
      top.emplace_back(max, f.Name.str());
      std::sort(top.begin(), top.end(), [](auto &a, auto &b) {
        return a.first > b.first || (a.first == b.first && a.second < b.second);
      });
      if (top.size() > topn)
        top.pop_back();
    }
    if (all) {
      if (!shown++)
        os << "Counters:\n";
      os << "  " << f.Name << ":\n"
         << "    Hash: " << llvm::format("0x%016" PRIx64, f.Hash) << "\n"
         << "    Counters: " << f.Counts.size() << "\n";
    }
  }
  if (r.hasError())
    return fail(err, std::string(path) + ": " + llvm::toString(r.getError()));
  auto summary = builder.getSummary();
  os << "Instrumentation level: " << (r.isIRLevelProfile() ? "IR" : "Front-end");
  if (r.isIRLevelProfile())
    os << "  entry_first = " << r.instrEntryBBEnabled();
  os << "\n";
  if (all)
    os << "Functions shown: " << shown << "\n";
  os << "Total functions: " << summary->getNumFunctions() << "\n";
  if (cs)
    os << "Context-sensitive records: " << cs << "\n";
  os << "Maximum function count: " << summary->getMaxFunctionCount() << "\n"
     << "Maximum internal block count: " << summary->getMaxInternalCount()
     << "\n";
  if (topn) {
    os << "Top " << topn << " functions with the largest internal block counts:\n";
    for (auto &[count, name] : top)
      os << "  " << name << ", max count = " << count << "\n";
  }
  put(out, text);
  return 0;
}

// Import library for module definition `def` (mingw-w64 syntax) and COFF
// machine `machine`; x86 names are decorated and import undecorated
// (llvm-dlltool -k).
int rok_llvm_import_lib(const char *def, size_t len, uint16_t machine_type,
                        const char *path, RokBuf *err) {
  auto machine = static_cast<llvm::COFF::MachineTypes>(machine_type);
  bool x86 = machine == llvm::COFF::IMAGE_FILE_MACHINE_I386;
  auto parsed = llvm::object::parseCOFFModuleDefinition(
      llvm::MemoryBufferRef(llvm::StringRef(def, len), path), machine, true,
      x86);
  if (!parsed)
    return fail(err, llvm::toString(parsed.takeError()));
  if (x86)
    for (auto &e : parsed->Exports) {
      if (!e.ImportName.empty() || (!e.Name.empty() && e.Name[0] == '?'))
        continue;
      e.SymbolName = e.Name;
      e.Name = e.Name.substr(0, e.Name.find('@', 1));
    }
  if (auto e = llvm::object::writeImportLibrary(parsed->OutputFile, path,
                                                parsed->Exports, machine, true))
    return fail(err, llvm::toString(std::move(e)));
  return 0;
}

} // extern "C"
