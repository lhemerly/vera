import { digest } from "../workflow.js";
import { callBroker } from "../broker.js";
import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
const plugin = fileURLToPath(new URL("../index.js", import.meta.url));
const dsh = process.env.DSH_CLI;
const native = process.env.VERA_NATIVE_PLUGIN;
const binary = process.env.VERA_CONTRACT_BINARY;
const source =
  "module ContractFixture; effect Value(value:u64); transition emit_value(value:u64) reads [] writes [] ensure value_matches:eq(effect_arg(Value,0,value),value); body { emit Value(value); commit; }";
const text = (m) =>
  typeof m.content === "string" ? m.content : JSON.stringify(m.content);
const parsed = (m) => {
  const raw = text(m);
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
};
for (const brokerMode of [false, true]) test(
  `real DSH fresh authority/executor sessions ${brokerMode ? "through broker" : "in process"}, local fake provider only`,
  { skip: !dsh || !native || !binary },
  async () => {
    const home = await mkdtemp(join(tmpdir(), "vera-contract-native-"));
    let child,
      server,
      stdout = "",
      stderr = "",
      failure;
    const requests = [];
    let ownerStep = 0,
      execStep = 0,
      specStep = 0;
    let workflow;
    let calls = 0;
    const observations=[];
    try {
      server = createServer(async (req, res) => {
        try {
          let raw = "";
          for await (const c of req) raw += c;
          const r = JSON.parse(raw);
          if(req.url==="/reference/observe") {
            const content=JSON.stringify({result:{status:"completed",stdout:r.argv[0],stderr:"",exit_code:0}});
            const result={id:"runtime-"+observations.length,content,content_hash:digest(content),provenance:"independent local test reference",complete:true};
            observations.push({request:r,result});res.end(JSON.stringify(result));return;
          }
          const msgs = r.messages;
          const all = msgs.map(text).join("\n");
          const isExec = all.includes(
            "Implement against the frozen VERA contract below."
          );
          const isSpec =
            !isExec &&
            all.includes("You are the independent specification authority.");
          const role = isExec ? "executor" : isSpec ? "specification" : "owner";
          const names = r.tools?.map((t) => t.function.name) ?? [];
          requests.push({ role, names, messages: msgs });
          let call;
          if (role === "specification") {
            assert.ok(!all.includes("OWNER_SECRET_NOT_FOR_CHILD"));
            assert.ok(names.includes("structured_output"));
            assert.ok(!names.includes("bash"));
            assert.ok(!names.includes("vera_workflow_step"));
            assert.ok(names.includes("vera_authority_evidence"));
            assert.ok(names.includes("vera_authority_observe"));
            const step=specStep++;
            if (step === 0) call = ["vera_authority_evidence", {}];
            else if(step === 1) {
              const catalog=msgs.filter(m=>m.role==="tool").map(parsed).filter(Boolean).at(-1);
              assert.equal(catalog?.kind,"catalog",JSON.stringify(catalog));
              assert.equal(catalog.items[0].id,"reference");
              assert.equal(typeof catalog.items[0].content_hash,"string");
              assert.equal(catalog.items[0].content,undefined);
              call=["vera_authority_evidence",{evidence_id:catalog.items[0].id}];
            } else if(step===2) {
              const snapshot=msgs.filter(m=>m.role==="tool").map(parsed).filter(Boolean).at(-1);
              assert.equal(snapshot?.kind,"snapshot");assert.equal(snapshot.id,"reference");
              call=["vera_authority_observe",{argv:["0"]}];
            } else if(step===3) {
              call=["structured_output",{outcome:"needs_evidence",discovery:{blocker:{kind:"none",detail:"",local_scope_impact:"",evidence_ids:[]},missing_evidence:["Need next permitted contrasting input"],candidate_assessments:[],proposed_probes:[{purpose:"Compare next input",request:{argv:["1"],stdin:"",fixtures:[]}}]}}];
            } else if(step===4) {
              assert.match(all,/Premature evidence deferral/);
              call=["vera_authority_observe",{argv:["1"],stdin:"",fixtures:[{path:"input.txt",content:"fixture"}]}];
            } else {
              assert.match(all, /independent reference behavior/);
              assert.equal(observations.length,2);
              call = [
                "structured_output",
                {
                  source,
                  core_scope:{guarantee:"Input value equals emitted payload",domain:"u64 value",assumptions:[],input_mapping:[{parameter:"value",origin:"input argument",role:"data"}],host_obligations:["Parse argument and render emitted payload"],unguaranteed_behavior:["Other host behavior"],out_of_domain_behavior:"Host handles other input syntax without truncation",equivalence_claim:"local_guarantee_only"},
                  discovery:{blocker:{kind:"none",detail:"",local_scope_impact:"",evidence_ids:[]},candidate_assessments:[],missing_evidence:[],proposed_probes:[]},
                  outcome:"ready",rationale:"Identity mapping belongs to emitted output",
                  grounding:{obligation_id:"R1",target_scope:"this_executable",input_origin:"input value argument",callsite:"emit command after parsing",observable_consumer:"stdout emitted payload",contrast:[{evidence_id:"runtime-0",observed_output:"0"},{evidence_id:"runtime-1",observed_output:"1"}]},
                  requirements_map: "Input value equals emitted payload",
                  coverage: [
                    {
                      id: "R1",
                      requirement: "emit value",
                      disposition: "core_contract",
                      justification: "reference observation",
                      evidence_ids: ["reference"],
                    },
                  ],
                  assumptions: "u64 bounded domain",
                  acceptance_tests: "Compare actual emitted effect",
                },
              ];
            }
          } else if (role === "executor") {
            assert.ok(!all.includes("OWNER_SECRET_NOT_FOR_CHILD"));
            assert.ok(names.includes("vera_contract_verify"));
            assert.ok(!names.includes("vera_verify"));
            assert.ok(!names.includes("vera_authority_evidence"));
            assert.ok(!names.includes("vera_authority_observe"));
            assert.ok(!names.includes("bash"));
            if (execStep === 0) call = ["bash", { command: "exit 97" }];
            else if (execStep === 1) {
              assert.match(
                all,
                /unknown|not found|denied|unavailable|not registered/i
              );
              call = [
                "vera_contract_verify",
                {
                  source: source.replace(
                    "eq(effect_arg(Value,0,value),value)",
                    "true"
                  ),
                },
              ];
            } else if (execStep === 2) {
              assert.match(all, /Contract mismatch/);
              call = [
                "vera_contract_verify",
                {
                  source: source.replace("emit Value(value)", "emit Value(0)"),
                },
              ];
            } else {
              assert.match(all, /counterexample/);
              assert.match(all, /specification_footprint/);
              call = [
                "structured_output",
                {
                  kind: "implementation",
                  source,
                  explanation: "Corrected emitted value using bound evidence",
                  evidence_ids: [],
                },
              ];
            }
            execStep++;
          } else {
            if (ownerStep === 0)
              call = [
                "vera_workflow_start",
                { requirements: "Emit supplied u64 exactly" },
              ];
            else if (ownerStep === 1) {
              const result = msgs
                .filter((m) => m.role === "tool")
                .map(parsed)
                .filter(Boolean)
                .at(-1);
              assert.ok(result?.id, JSON.stringify(result));
              workflow = result.id;
              call = ["vera_workflow_step", { id: workflow }];
            } else {
              assert.match(all, /admitted/);
              assert.match(all, /artifact_hash/);
            }
            ownerStep++;
          }
          res.writeHead(200, { "content-type": "text/event-stream" });
          calls++;
          const chunk = (delta, finish_reason = null) =>
            res.write(
              "data: " +
                JSON.stringify({
                  id: "local-" + calls,
                  object: "chat.completion.chunk",
                  created: 1,
                  model: "fixture",
                  choices: [{ index: 0, delta, finish_reason }],
                }) +
                "\n\n"
            );
          chunk({
            role: "assistant",
            ...(call
              ? {
                  tool_calls: [
                    {
                      index: 0,
                      id: "call-" + calls,
                      type: "function",
                      function: {
                        name: call[0],
                        arguments: JSON.stringify(call[1]),
                      },
                    },
                  ],
                }
              : { content: "Fixture complete." }),
          });
          chunk({}, call ? "tool_calls" : "stop");
          res.end("data: [DONE]\n\n");
        } catch (e) {
          failure = e;
          res.writeHead(500);
          res.end(String(e));
        }
      });
      await new Promise((r) => server.listen(0, "127.0.0.1", r));
      await mkdir(join(home, "profiles/headless"), { recursive: true });
      await mkdir(join(home, "agents"));
      await mkdir(join(home, "workspace"));
      await mkdir(join(home, "state"), { mode: 0o700 });
      await writeFile(
        join(home, "task.json"),
        JSON.stringify({
          id: "fixture",
          original_requirements: "Emit input value",
          provenance: "test fixture",
          policy:{requireRuntimeGrounding:true},
          obligations:[{id:"R1",requirement:"emit value"}],
          evidence: [
            {
              id: "reference",
              provenance: "host captured original binary",
              content:
                "independent reference behavior: input equals emitted value",
            },
          ],
        })
      );
      await writeFile(
        join(home, "profiles/headless/package.json"),
        JSON.stringify({
          name: "dsh-profile-headless",
          private: true,
          dependencies: {},
          dsh: {
            profile: {
              bundles: ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
            },
          },
        })
      );
      const patch = [
        "session-title-llm",
        "llm-retry",
        "compaction-basic",
        "command-compact",
        "plugin-manager",
        "tool-plugin-manager",
        "tool-subagent",
        "tool-subagent-fork",
        "tool-subagent-control",
        "tool-subagent-list-agents",
        "tool-workflow",
        "workflow-ptc",
        "credentials",
        "authorization",
        "deepseek-account",
        "tool-web",
      ].map((id) => ({ id, disabled: true }));
      if (brokerMode) patch.push({id:"headless-runner",disabled:true});
      patch.push(
        {
          id: "llm-pi-ai",
          config: {
            providers: {
              fixture: {
                api: "openai-completions",
                apiKeyEnv: "FIXTURE_KEY",
                baseURL: `http://127.0.0.1:${server.address().port}/v1`,
                models: [
                  {
                    id: "fixture",
                    name: "fixture",
                    input: ["text"],
                    contextWindow: 262144,
                    maxTokens: 4096,
                  },
                ],
                retryPolicy: { mode: "normal", maxRetries: 0 },
              },
            },
          },
        },
        {
          id: "agent-default-model",
          config: { provider: "fixture", model: "fixture" },
        },
        { id: "tools", config: { mode: "native" } },
        {
          id: "skill-filesystem",
          config: {
            includeDefaultRoots: false,
            customSkillDirs: [],
            watch: false,
          },
        },
        {
          insert: [
            {
              id: "vera",
              name: native,
              config: { binary, node: process.execPath },
            },
            {
              id: "vera-contract-workflow",
              name: plugin,
              config: {
                binary,
                stateDirectory: join(home, "state"),
                taskManifest: join(home, "task.json"),
                provider: "spawn",
                observerURL:`http://127.0.0.1:${server.address().port}`,
                ...(brokerMode ? {brokerSocket:join(home,"authority.sock"),actorUid:process.getuid()+1} : {}),
              },
            },
          ],
        }
      );
      await writeFile(join(home, "patch.json"), JSON.stringify(patch));
      child = spawn(
        process.execPath,
        [dsh, "headless", "--patch", join(home, "patch.json"), "--json", "-"],
        {
          cwd: join(home, "workspace"),
          env: {
            PATH: process.env.PATH,
            HOME: home,
            DSH_HOME: home,
            DSH_AGENTS_HOME: join(home, "agents"),
            DSH_TELEMETRY_DISABLED: "1",
            DSH_TOOLS_MODE: "native",
            FIXTURE_KEY: "local-not-real",
          },
          stdio: ["pipe", "pipe", "pipe"],
        }
      );
      child.stdin.end(
        "OWNER_SECRET_NOT_FOR_CHILD. Run local contract fixture."
      );
      child.stdout.on("data", (b) => (stdout += b));
      child.stderr.on("data", (b) => (stderr += b));
      if (brokerMode) {
        let started=false;
        for(let i=0;i<200;i++) {
          if(child.exitCode!==null)throw Error("Broker exited: "+stderr);
          try { await callBroker(join(home,"authority.sock"),"vera_task_get",{}); started=true; break; }
          catch(e) { if(!['ENOENT','ECONNREFUSED'].includes(e.code))throw e; }
          await new Promise(r=>setTimeout(r,25));
        }
        assert.ok(started,"Broker readiness: "+stderr);
        const begun=await callBroker(join(home,"authority.sock"),"vera_workflow_start",{requirements:"Emit supplied u64 exactly"});
        const result=await callBroker(join(home,"authority.sock"),"vera_workflow_step",{id:begun.id});
        assert.match(JSON.stringify(result),/admitted/);
        assert.equal(result.admitted.source,source);
        assert.equal(digest(result.admitted.source),result.admitted.receipt.source_hash);
        assert.equal(ownerStep,0,"No supervisor inference permitted");
        if(failure)throw failure;
      } else {
        const timer = setTimeout(() => child.kill("SIGKILL"), 120000);
        const code = await new Promise((r, e) => { child.once("close", r); child.once("error", e); });
        clearTimeout(timer);
        if (failure) throw failure;
        assert.equal(code, 0, stderr + "\n" + stdout);
        assert.equal(ownerStep, 3, stderr + "\n" + stdout);
      }
      assert.equal(execStep, 4);
      assert.equal(
        requests.filter((r) => r.role === "specification").length,
        6
      );
      const log = (await readFile(join(home, "state/audit.jsonl"), "utf8"))
        .trim()
        .split("\n")
        .map(JSON.parse);
      const reserved = log.filter((e) => e.type === "role_reserved");
      assert.equal(reserved.length, 2);
      assert.notEqual(reserved[0].child_id, reserved[1].child_id);
      assert.equal(log.filter((e) => e.type === "role_closed").length, 2);
      assert.ok(log.some((e) => e.type === "artifact_admitted"));
      if(brokerMode) assert.ok(log.some(e=>e.type==="broker_response" && e.method==="vera_workflow_step"));
      assert.ok(log.some((e) => e.type === "authority_observed"));
      assert.equal(log.filter(e=>e.type==="runtime_observation").length,2);
      assert.deepEqual(observations[1].request,{argv:["1"],stdin:"",fixtures:[{path:"input.txt",content:"fixture"}]});
      console.log(
        JSON.stringify({
          paidCalls: 0,
          localFixtureRequests: calls,
          distinctChildSessions: reserved.map((e) => e.child_id),
          admitted: true,
        })
      );
    } catch (e) {
      console.error(stderr);
      console.error(stdout);
      throw e;
    } finally {
      child?.kill("SIGKILL");
      if (server) await new Promise((r) => server.close(r));
      await rm(home, { recursive: true, force: true });
    }
  }
);
