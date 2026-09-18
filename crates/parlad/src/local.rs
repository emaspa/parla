//! The local model: a GGUF instruct model on this machine's GPU, shared by
//! the judged path (which scores options) and dictation cleanup (which
//! generates text).
//!
//! Every question the judge asks has a closed set of answers, so there the
//! model never generates: for each option we compute the probability the model
//! assigns to *exactly that option* as its reply, and normalise over the
//! set. The probability of an option is the product of its tokens'
//! probabilities followed by the end-of-turn token, so `a:1` and `a:12` are
//! disjoint outcomes rather than one being a prefix of the other. The
//! result has the shape TypeSafe returns: a probability per option, and one
//! per yes/no question.
//!
//! The cost is prefill only. The state is rendered once per utterance and
//! shared through the KV cache across every question (the longest common
//! token prefix with the previous prompt is kept, the rest re-decoded), and
//! the options of one question are scored in one batch as parallel
//! sequences forked from the prompt. No token is sampled on that path.
//!
//! Cleanup and voice edits do generate: greedy decoding, one token per
//! step, until the model ends its turn or the token budget runs out. The
//! same cache-sharing applies, so a prompt that repeats its system text and
//! dictionary only pays for the transcript.
//!
//! llama.cpp contexts are not `Send`, so the model lives on one thread that
//! takes jobs from a channel; [`LocalModel`] is the handle the async side
//! holds.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::mpsc;
use std::time::Duration;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::token::LlamaToken;

use crate::config::LocalConfig;
use crate::oracle::{Answer, NoulCriteria, Prose, Question, Response, Usage};

/// Sequences a context may hold at once: the prompt plus this many minus
/// one options scored in parallel. Only bookkeeping with a unified cache.
const SEQUENCES: u32 = 64;
/// Tokens per decode call. Bounds the working memory of one forward pass.
const BATCH: u32 = 1024;

const JUDGE_SYSTEM_PROMPT: &str =
    "You judge what a user said to a voice assistant that controls a KDE Plasma \
desktop. You are shown the desktop's observed state as JSON and one question about the \
utterance. Reply with exactly one of the allowed answers and nothing else.";

pub struct LocalModel {
    /// Dropped first, so the thread's loop ends and it frees the model
    /// while CUDA is still up; freeing at process exit aborts instead.
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    name: String,
}

/// What a generation produced.
#[derive(Debug, Clone)]
pub struct Generated {
    pub text: String,
    pub usage: Usage,
}

impl Drop for LocalModel {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Job {
    Evaluate {
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<Response>>,
    },
    Generate {
        system: String,
        user: String,
        max_tokens: u32,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<Generated>>,
    },
}

impl LocalModel {
    /// Load the model onto the GPU and start its thread. Blocks for the
    /// load (seconds for a few GB), so call it off the async runtime.
    pub fn load(cfg: &LocalConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg.model_path.exists(),
            "local model not found at {} (fetch it: scripts/fetch-model.sh)",
            cfg.model_path.display()
        );
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (loaded_tx, loaded_rx) = mpsc::sync_channel::<anyhow::Result<String>>(1);
        let cfg = cfg.clone();
        let thread = std::thread::Builder::new()
            .name("parla-llm".into())
            .spawn(move || serve(&cfg, &loaded_tx, &job_rx))?;
        let name = loaded_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("local model thread exited before loading"))??;
        Ok(Self {
            jobs: Some(jobs),
            thread: Some(thread),
            name,
        })
    }

    /// The model file's stem, for log lines.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Score every question's options. Jobs run one at a time on the model
    /// thread, so `timeout` covers queueing behind another job too.
    pub async fn evaluate(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        timeout: Duration,
    ) -> anyhow::Result<Response> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.submit(
            Job::Evaluate {
                state: state.clone(),
                questions: questions.clone(),
                reply,
            },
            rx,
            timeout,
        )
        .await
    }

    /// Generate a reply to one user turn, greedily, up to `max_tokens`.
    pub async fn generate(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> anyhow::Result<Generated> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.submit(
            Job::Generate {
                system: system.to_string(),
                user: user.to_string(),
                max_tokens,
                reply,
            },
            rx,
            timeout,
        )
        .await
    }

    async fn submit<T>(
        &self,
        job: Job,
        rx: tokio::sync::oneshot::Receiver<anyhow::Result<T>>,
        timeout: Duration,
    ) -> anyhow::Result<T> {
        self.jobs
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("local model is shut down"))?
            .send(job)
            .map_err(|_| anyhow::anyhow!("local model thread is gone"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => anyhow::bail!("local model thread dropped the job"),
            Err(_) => anyhow::bail!("local model: no answer within {} ms", timeout.as_millis()),
        }
    }
}

/// The model thread: load, report, then answer jobs until the handle drops.
fn serve(
    cfg: &LocalConfig,
    loaded: &mpsc::SyncSender<anyhow::Result<String>>,
    jobs: &mpsc::Receiver<Job>,
) {
    let (backend, model) = match load_model(cfg) {
        Ok(m) => m,
        Err(e) => {
            let _ = loaded.send(Err(e));
            return;
        }
    };
    let mut worker = match Worker::new(&backend, &model, cfg) {
        Ok(w) => w,
        Err(e) => {
            let _ = loaded.send(Err(e));
            return;
        }
    };
    let name = cfg
        .model_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".into());
    if loaded.send(Ok(name)).is_err() {
        return;
    }
    // A caller that timed out has dropped its receiver; nothing to do then.
    for job in jobs {
        match job {
            Job::Evaluate {
                state,
                questions,
                reply,
            } => {
                let _ = reply.send(worker.evaluate(&state, &questions));
            }
            Job::Generate {
                system,
                user,
                max_tokens,
                reply,
            } => {
                let _ = reply.send(worker.generate(&system, &user, max_tokens));
            }
        }
    }
}

fn load_model(cfg: &LocalConfig) -> anyhow::Result<(LlamaBackend, LlamaModel)> {
    // Route ggml/llama.cpp's stderr chatter through tracing like whisper's.
    llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());
    let backend = LlamaBackend::init().map_err(|e| anyhow::anyhow!("llama backend: {e}"))?;
    if cfg.gpu_layers > 0 && !backend.supports_gpu_offload() {
        tracing::warn!(
            "no GPU backend: the local model will run on the CPU \
             (built with the cuda feature; check the CUDA driver and libraries)"
        );
    }
    let params = LlamaModelParams::default().with_n_gpu_layers(cfg.gpu_layers);
    let t0 = std::time::Instant::now();
    let model = LlamaModel::load_from_file(&backend, &cfg.model_path, &params)
        .map_err(|e| anyhow::anyhow!("failed to load local model: {e}"))?;
    tracing::info!(
        "loaded local model {} in {:.1}s ({} layers, {} params)",
        cfg.model_path.display(),
        t0.elapsed().as_secs_f32(),
        model.n_layer(),
        model.n_params(),
    );
    Ok((backend, model))
}

/// One loaded model with its context and the tokens currently in the
/// cache's sequence 0.
struct Worker<'m> {
    model: &'m LlamaModel,
    ctx: LlamaContext<'m>,
    template: Option<LlamaChatTemplate>,
    add_bos: bool,
    eos: LlamaToken,
    /// Tokens sequence 0 holds, so the next prompt re-decodes only its
    /// unshared suffix.
    cached: Vec<LlamaToken>,
    n_ctx: usize,
}

/// A question rendered for the model, with the option strings to score.
struct Rendered {
    prompt: String,
    options: Vec<String>,
}

impl<'m> Worker<'m> {
    fn new(
        backend: &LlamaBackend,
        model: &'m LlamaModel,
        cfg: &LocalConfig,
    ) -> anyhow::Result<Self> {
        let n_ctx = NonZeroU32::new(cfg.context_tokens)
            .ok_or_else(|| anyhow::anyhow!("local.context_tokens must be > 0"))?;
        let threads = i32::try_from(cfg.threads.max(1)).unwrap_or(i32::MAX);
        let params = LlamaContextParams::default()
            .with_n_ctx(Some(n_ctx))
            .with_n_batch(BATCH)
            .with_n_ubatch(BATCH)
            .with_n_seq_max(SEQUENCES)
            // One pool of cells shared by every sequence, so forking the
            // prompt into option sequences tags cells instead of copying.
            .with_kv_unified(true)
            .with_n_threads(threads)
            .with_n_threads_batch(threads)
            .with_no_perf(true);
        let ctx = model
            .new_context(backend, params)
            .map_err(|e| anyhow::anyhow!("local model context: {e}"))?;
        let template = match model.chat_template(None) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("local model has no chat template ({e}); using a plain layout");
                None
            }
        };
        // GGUF says whether this vocabulary expects a leading BOS (Llama does,
        // Qwen does not); the chat template output never carries it.
        let add_bos = model
            .meta_val_str("tokenizer.ggml.add_bos_token")
            .map(|v| v == "true")
            .unwrap_or(false);
        Ok(Self {
            model,
            ctx,
            template,
            add_bos,
            eos: model.token_eos(),
            cached: Vec::new(),
            n_ctx: n_ctx.get() as usize,
        })
    }

    fn evaluate(
        &mut self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> anyhow::Result<Response> {
        let state_text = serde_json::to_string_pretty(state)?;
        let mut answers = BTreeMap::new();
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        for (id, q) in questions {
            let rendered = render(&state_text, q);
            tracing::trace!("question {id} as rendered:\n{}", rendered.prompt);
            let prompt = self.wrap(JUDGE_SYSTEM_PROMPT, &rendered.prompt)?;
            let logp = self.score(&prompt, &rendered.options, &mut usage)?;
            let probs = softmax(&logp);
            tracing::trace!(
                "question {id} scores: {}",
                rendered
                    .options
                    .iter()
                    .zip(&probs)
                    .map(|(o, p)| format!("{o} {p:.3}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let answer =
                match q {
                    Question::Noul { .. } => Answer::Noul { noul: probs[0] },
                    Question::Choice { .. } => {
                        let (best, _) = probs.iter().enumerate().fold(
                            (0, f64::NEG_INFINITY),
                            |acc, (i, &p)| {
                                if p > acc.1 {
                                    (i, p)
                                } else {
                                    acc
                                }
                            },
                        );
                        Answer::Choice {
                            choice: rendered.options[best].clone(),
                            confidence: probs[best],
                            probabilities: rendered
                                .options
                                .iter()
                                .cloned()
                                .zip(probs.iter().copied())
                                .collect(),
                        }
                    }
                    Question::Score { .. } => Answer::Unknown,
                };
            answers.insert(id.clone(), answer);
        }
        Ok(Response {
            answers,
            usage: Some(usage),
        })
    }

    /// The model's chat layout around one user turn, ending where the
    /// assistant's reply starts.
    fn wrap(&self, system: &str, user: &str) -> anyhow::Result<String> {
        match &self.template {
            Some(t) => {
                let chat = [
                    LlamaChatMessage::new("system".into(), system.into())?,
                    LlamaChatMessage::new("user".into(), user.into())?,
                ];
                Ok(self.model.apply_chat_template(t, &chat, true)?)
            }
            None => Ok(format!("{system}\n\n{user}\n\nAnswer:")),
        }
    }

    /// Greedy decoding after `user`, until the model ends its turn or
    /// `max_tokens` is reached. The prompt's shared prefix with whatever the
    /// cache holds is reused, as for scoring.
    fn generate(&mut self, system: &str, user: &str, max_tokens: u32) -> anyhow::Result<Generated> {
        let prompt = self.wrap(system, user)?;
        let add_bos = if self.add_bos {
            AddBos::Always
        } else {
            AddBos::Never
        };
        let tokens = self.model.str_to_token(&prompt, add_bos)?;
        anyhow::ensure!(!tokens.is_empty(), "empty prompt");
        anyhow::ensure!(
            (tokens.len() + max_tokens as usize) < self.n_ctx,
            "prompt of {} tokens plus {max_tokens} to generate does not fit local.context_tokens ({})",
            tokens.len(),
            self.n_ctx
        );
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let t0 = std::time::Instant::now();
        let mut logits = self.prefill(&tokens, &mut usage)?;
        let prefill_ms = t0.elapsed().as_millis();
        let mut bytes: Vec<u8> = Vec::new();
        let mut batch = LlamaBatch::new(1, 1);
        while usage.output_tokens < max_tokens {
            let next = argmax(&logits);
            if next == self.eos || self.model.is_eog_token(next) {
                break;
            }
            bytes.extend(self.model.token_to_piece_bytes(next, 64, false, None)?);
            usage.output_tokens += 1;
            batch.clear();
            batch.add(next, self.cached.len() as i32, &[0], true)?;
            self.ctx.decode(&mut batch)?;
            self.cached.push(next);
            logits = self.ctx.get_logits_ith(0).to_vec();
        }
        tracing::debug!(
            "generated {} tokens in {:.0}ms after {prefill_ms}ms prefill ({} prompt tokens decoded)",
            usage.output_tokens,
            t0.elapsed().as_secs_f64() * 1000.0,
            usage.input_tokens,
        );
        Ok(Generated {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            usage,
        })
    }

    /// Log-probability that the model replies with each option (its tokens
    /// then end-of-turn), given `prompt`. Unnormalised across options.
    fn score(
        &mut self,
        prompt: &str,
        options: &[String],
        usage: &mut Usage,
    ) -> anyhow::Result<Vec<f64>> {
        let add_bos = if self.add_bos {
            AddBos::Always
        } else {
            AddBos::Never
        };
        let prompt_tokens = self.model.str_to_token(prompt, add_bos)?;
        let option_tokens: Vec<Vec<LlamaToken>> = options
            .iter()
            .map(|o| self.model.str_to_token(o, AddBos::Never))
            .collect::<Result<_, _>>()?;
        anyhow::ensure!(
            !prompt_tokens.is_empty() && option_tokens.iter().all(|t| !t.is_empty()),
            "empty prompt or option"
        );
        let longest = option_tokens.iter().map(Vec::len).max().unwrap_or(0);
        anyhow::ensure!(
            prompt_tokens.len() + longest < self.n_ctx,
            "prompt of {} tokens does not fit judge.local.context_tokens ({})",
            prompt_tokens.len(),
            self.n_ctx
        );

        let n_vocab = usize::try_from(self.model.n_vocab())?;
        let base = self.prefill(&prompt_tokens, usage)?;
        let base_lse = log_sum_exp(&base);
        let first: Vec<f64> = option_tokens
            .iter()
            .map(|t| f64::from(base[t[0].0 as usize % n_vocab]) - base_lse)
            .collect();
        drop(base);

        let start = prompt_tokens.len() as i32;
        let mut logp = first;
        // Score each option's continuation on its own sequence forked from
        // the prompt. `SEQUENCES - 1` options per round; each round is one
        // or more decodes of at most BATCH tokens.
        let per_round = (SEQUENCES - 1) as usize;
        let mut batch = LlamaBatch::new(BATCH as usize, 1);
        for (round, chunk) in option_tokens.chunks(per_round).enumerate() {
            let offset = round * per_round;
            for s in 1..=chunk.len() as i32 {
                self.ctx.copy_kv_cache_seq(0, s, None, None)?;
            }
            // (batch index, option index, token expected next)
            let mut pending: Vec<(i32, usize, LlamaToken)> = Vec::new();
            batch.clear();
            for (k, tokens) in chunk.iter().enumerate() {
                let seq = k as i32 + 1;
                for (j, &tok) in tokens.iter().enumerate() {
                    if batch.n_tokens() as u32 == BATCH {
                        self.flush(&mut batch, &pending, &mut logp)?;
                        pending.clear();
                    }
                    let next = tokens.get(j + 1).copied().unwrap_or(self.eos);
                    pending.push((batch.n_tokens(), offset + k, next));
                    batch.add(tok, start + j as i32, &[seq], true)?;
                }
            }
            if batch.n_tokens() > 0 {
                self.flush(&mut batch, &pending, &mut logp)?;
            }
            usage.output_tokens += chunk.iter().map(|t| t.len() as u32).sum::<u32>();
            for s in 1..=chunk.len() as i32 {
                self.ctx.clear_kv_cache_seq(Some(s as u32), None, None)?;
            }
        }
        Ok(logp)
    }

    /// Bring sequence 0 to hold exactly `tokens`, re-decoding only what the
    /// cache does not already share, and return the logits after the last
    /// token.
    fn prefill(&mut self, tokens: &[LlamaToken], usage: &mut Usage) -> anyhow::Result<Vec<f32>> {
        let shared = common_prefix(&self.cached, tokens);
        // Logits are only kept for the tokens of the last decode, so the
        // final prompt token is always re-decoded.
        let keep = shared.min(tokens.len() - 1);
        self.ctx.kv_cache_seq_rm(0, Some(keep as u32), None)?;
        self.cached.truncate(keep);

        let mut batch = LlamaBatch::new(BATCH as usize, 1);
        let mut logits = None;
        for chunk in tokens[keep..].chunks(BATCH as usize) {
            batch.clear();
            let last = chunk.len() - 1;
            for (i, &tok) in chunk.iter().enumerate() {
                let pos = (self.cached.len() + i) as i32;
                batch.add(tok, pos, &[0], i == last)?;
            }
            self.ctx.decode(&mut batch)?;
            self.cached.extend_from_slice(chunk);
            logits = Some(self.ctx.get_logits_ith(last as i32).to_vec());
        }
        usage.input_tokens += (tokens.len() - keep) as u32;
        logits.ok_or_else(|| anyhow::anyhow!("nothing to decode"))
    }

    /// Decode one batch of option tokens and fold the log-probability of
    /// each expected next token into its option's total.
    fn flush(
        &mut self,
        batch: &mut LlamaBatch,
        pending: &[(i32, usize, LlamaToken)],
        logp: &mut [f64],
    ) -> anyhow::Result<()> {
        self.ctx.decode(batch)?;
        let n_vocab = usize::try_from(self.model.n_vocab())?;
        for &(i, option, next) in pending {
            let l = self.ctx.get_logits_ith(i);
            logp[option] += f64::from(l[next.0 as usize % n_vocab]) - log_sum_exp(l);
        }
        batch.clear();
        Ok(())
    }
}

/// Lay one question out for the model: the state, the instructions, the
/// allowed answers. Yes/no questions become the two-option choice
/// `yes`/`no`, in that order, so the caller reads the first probability.
fn render(state: &str, q: &Question) -> Rendered {
    let mut text = format!("Desktop state (JSON):\n{state}\n\n");
    match q {
        Question::Noul {
            instructions,
            criteria,
        } => {
            text.push_str("Question: ");
            text.push_str(&prose(instructions));
            text.push('\n');
            if let Some(NoulCriteria { yes, no }) = criteria {
                text.push_str(&format!("Answer yes if: {yes}\nAnswer no if: {no}\n"));
            }
            text.push_str("\nAllowed answers: yes, no\nReply with one word.");
            Rendered {
                prompt: text,
                options: vec!["yes".into(), "no".into()],
            }
        }
        Question::Choice {
            instructions,
            criteria,
        } => {
            text.push_str("Question: ");
            text.push_str(&prose(instructions));
            text.push_str("\n\nAllowed answers, one per line as `key: meaning`:\n");
            for (key, desc) in criteria.iter() {
                text.push_str(&format!("{key}: {}\n", prose(desc).replace('\n', " ")));
            }
            text.push_str("\nReply with the key only.");
            Rendered {
                prompt: text,
                options: criteria.keys().cloned().collect(),
            }
        }
        Question::Score {
            instructions,
            criteria,
        } => {
            text.push_str("Question: ");
            text.push_str(&prose(instructions));
            text.push_str("\n\nAllowed answers, one per line as `level: meaning`:\n");
            for (i, desc) in criteria.iter().enumerate() {
                text.push_str(&format!("{}: {}\n", i + 1, prose(desc).replace('\n', " ")));
            }
            text.push_str("\nReply with the level only.");
            Rendered {
                prompt: text,
                options: (1..=criteria.len()).map(|i| i.to_string()).collect(),
            }
        }
    }
}

/// Plain text for a question's prose: strings as they are, objects as
/// `key: value` lines, arrays comma-separated.
fn prose(v: &Prose) -> String {
    match v {
        Prose::String(s) => s.clone(),
        Prose::Array(items) => items.iter().map(prose).collect::<Vec<_>>().join(", "),
        Prose::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {}", prose(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        Prose::Null => String::new(),
        other => other.to_string(),
    }
}

/// The most likely next token.
fn argmax(logits: &[f32]) -> LlamaToken {
    let (i, _) = logits
        .iter()
        .enumerate()
        .fold(
            (0, f32::NEG_INFINITY),
            |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc },
        );
    LlamaToken(i as i32)
}

fn common_prefix(a: &[LlamaToken], b: &[LlamaToken]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn log_sum_exp(logits: &[f32]) -> f64 {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !max.is_finite() {
        return f64::from(max);
    }
    let sum: f64 = logits.iter().map(|&x| f64::from(x - max).exp()).sum();
    f64::from(max) + sum.ln()
}

fn softmax(logp: &[f64]) -> Vec<f64> {
    let max = logp.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = logp.iter().map(|&x| (x - max).exp()).collect();
    let total: f64 = weights.iter().sum();
    weights.into_iter().map(|w| w / total).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::Criteria;
    use serde_json::json;

    #[test]
    fn softmax_normalises_and_orders() {
        let p = softmax(&[-1.0, -3.0, -1.0]);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(p[0] > p[1] && (p[0] - p[2]).abs() < 1e-12);
    }

    #[test]
    fn log_sum_exp_matches_direct_computation() {
        let l = [1.0f32, 2.0, 3.0];
        let direct = (1f64.exp() + 2f64.exp() + 3f64.exp()).ln();
        assert!((log_sum_exp(&l) - direct).abs() < 1e-6);
    }

    #[test]
    fn prose_flattens_objects_and_arrays() {
        let v = json!({"task": "Decide", "examples": ["open firefox", "close it"]});
        assert_eq!(prose(&v), "examples: open firefox, close it\ntask: Decide");
        assert_eq!(prose(&json!("plain")), "plain");
    }

    #[test]
    fn noul_renders_yes_no_in_that_order() {
        let q = Question::Noul {
            instructions: json!("Is it prose?"),
            criteria: Some(NoulCriteria {
                yes: "text".into(),
                no: "a command".into(),
            }),
        };
        let r = render("{}", &q);
        assert_eq!(r.options, vec!["yes", "no"]);
        assert!(r.prompt.contains("Answer yes if: text"));
        assert!(r.prompt.contains("Allowed answers: yes, no"));
    }

    #[test]
    fn choice_options_follow_the_criteria_order() {
        let q = Question::Choice {
            instructions: json!({"task": "pick"}),
            criteria: Criteria::from([
                ("a:0", json!("installed application Firefox")),
                ("__none__", json!("nothing")),
            ]),
        };
        let r = render("{}", &q);
        assert_eq!(r.options, vec!["a:0", "__none__"]);
        assert!(r.prompt.contains("a:0: installed application Firefox\n"));
        assert!(r.prompt.contains("task: pick"));
    }

    #[test]
    fn common_prefix_stops_at_first_difference() {
        let a: Vec<LlamaToken> = [1, 2, 3].map(LlamaToken::new).to_vec();
        let b: Vec<LlamaToken> = [1, 2, 4, 5].map(LlamaToken::new).to_vec();
        assert_eq!(common_prefix(&a, &b), 2);
        assert_eq!(common_prefix(&[], &b), 0);
    }
}
