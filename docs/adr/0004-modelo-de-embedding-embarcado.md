# ADR 0004 — Modelo de embedding embarcado (MiniLM int8) + trait para BYO

**Status:** Aceito (jul/2026)

## Contexto

Gerar embeddings exige um modelo. Exigir API key de embedding externa quebraria as
promessas centrais do produto: local-first, "nada sai da máquina", instalação em um
comando, uso em ambiente air-gapped.

## Decisão

Embarcar **all-MiniLM-L6-v2 quantizado int8** (~23 MB, 384 dims) via `ort` (ONNX Runtime,
CPU-only), com tokenizer e vocab dentro do binário. Plugabilidade via
`trait Embedder { fn embed(&self, text: &str) -> Vec<f32>; fn id(&self) -> ModelId; }` —
trocar de modelo é config, não código. `model_id` + dims ficam gravados no header do
arquivo; misturar modelos no mesmo arquivo é erro, e a troca exige `embedmind reembed`.

## Alternativas rejeitadas

- **Exigir API de embedding (OpenAI/Voyage/etc.):** quebra "no API key" e air-gap; adiciona rede ao núcleo (proibido).
- **Modelo maior (melhor recall):** estoura o orçamento de binário < 40 MB e a latência de `remember` < 200 ms p99 em CPU comum.

## Consequências

- Instalação zero-dependência preservada; `ort` + `tokenizers` são as duas maiores deps, isoladas atrás do trait.
- Qualidade multilíngue (pt-BR) é risco monitorado no dogfooding — **[ABERTO]** no DESIGN §12: avaliar `bge-small`/modelo multilíngue; a troca não quebra arquitetura.
- `reembed` já nasce como caminho de upgrade de modelo (e semente da feature premium de reprocessamento/histórico).
