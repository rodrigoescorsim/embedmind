# CLAUDE.md — Contexto do projeto EmbedMind

Contexto para IA e colaboradores. Leia antes de propor ou implementar qualquer coisa.

## O que é este projeto

**EmbedMind** é uma engine embarcada de memória para agentes de IA (vetor + full-text + grafo, arquivo único, crash-safe, Rust), **empacotada e lançada como servidor MCP de memória + CLI** — não como "um banco de dados". É a aposta principal do plano open-core de 90 dias do founder (Rodrigo Escorsim, dev sênior C++/Rust, solo).

**Este é o repo de código** (privado até o fim do M1; a partir daqui, os documentos deste repo são a versão canônica). Documentos de origem do planejamento — fora deste repo, na pasta local `C:\workspace\Rodrigo\empreendedor_digital\ideias\`:

- `estrategias_negocio_github_2026_07.html` — card #1: tese, moat, prós/contras, tier enterprise.
- `plano_construcao_open_core_2026_07.html` — plano de execução de 90 dias (M0–M3), métricas de go/no-go.
- `projetos/EmbedMind/` — rascunhos originais destes documentos (snapshot histórico).

Documentos deste repo: [README.md](README.md) (público, pitch/UX), [ROADMAP.md](ROADMAP.md) (o quê/quando), [DESIGN.md](DESIGN.md) (**como** — formato de arquivo, WAL, HNSW, decisões técnicas com justificativa; consultar antes de implementar qualquer módulo da engine). Especificações e políticas: [docs/FORMAT.md](docs/FORMAT.md) (spec byte a byte do formato `.mind` — normativa a partir da v0.1), [docs/TESTING.md](docs/TESTING.md) (harness de crash, fuzzing, property tests, matriz de CI), [docs/BENCHMARKS.md](docs/BENCHMARKS.md) (metodologia dos benchmarks honestos), [docs/adr/](docs/adr/README.md) (decisões de arquitetura, um arquivo por decisão). Públicos de comunidade: [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), [CHANGELOG.md](CHANGELOG.md), [LICENSE](LICENSE) (MIT, núcleo).

## Decisões-chave (não reabrir sem motivo forte)

1. **Empacotamento: MCP server + CLI, não "database".** A porta de entrada é "memória persistente para seus agentes" (`remember`/`recall`/`forget`), plug-and-play. A engine gradua para crate/lib independente com bindings depois que amadurecer.
2. **A engine é o ativo; o MCP é casca descartável.** Se o protocolo MCP perder relevância, a engine sobrevive. Nunca deixar lógica de domínio vazar para a camada MCP.
3. **Open-core:** núcleo MIT completo e genuinamente útil. Premium segue as 4 classes: **histórico** (time-travel), **compliance** (criptografia, RBAC, auditoria, air-gap), **rastreabilidade** (proveniência plena), **integrações** (sync/equipe/conectores). Proveniência *básica* (qual agente/sessão gravou) é grátis — é a semente da rastreabilidade premium.
4. **Tier enterprise = tese ex-CodeVault reabsorvida:** o dev de banco/fintech descobre o repo, usa grátis; o compliance da empresa exige a versão paga (criptografia, RBAC, auditoria, air-gap). Self-service, **zero venda B2B ativa** — a empresa vem ao produto.
5. **Crash-safety antes de features.** WAL primeiro. Um único bug de corrupção de dados mata a marca — confiabilidade É o moat. Fuzzing e testes de crash no CI desde o início.
6. **Repo privado até o fim do M1** (v0.1 de ponta a ponta). Lançamento público coordenado na semana 5. Se no dia 45 ainda estiver privado, é alarme vermelho — lançar o que existir.
7. **Benchmark honesto** vs. sqlite-vec/zvec no README, mesmo perdendo em algo — honestidade técnica é diferencial de marca.
8. **QuantForge está excluída do portfólio** por decisão do founder; não sugerir pivôs nessa direção.

## Arquitetura (alvo)

```
crates/
  embedmind-core/    # a engine: formato de arquivo único, WAL, HNSW,
                     # full-text, (depois) grafo. Zero deps de rede. O ATIVO.
  embedmind-mcp/     # servidor MCP: remember/recall/forget +
                     # memória automática de contexto de projeto. Casca fina.
  embedmind-cli/     # CLI: serve, remember, recall, stats, export.
  (futuro) bindings/ # Python primeiro (destrava LangChain/agentes custom), TS depois.
```

- **Formato de arquivo:** único arquivo (`.mind`), WAL para durabilidade, projetado para compatibilidade futura — formato público não se quebra depois; mudanças exigem versionamento e migração.
- **Índices:** HNSW para vetor; full-text + filtros de metadados (M2); camada de grafo simples — entidades e relações entre memórias (M3).
- **Embeddings:** modelo ONNX quantizado embarcado, CPU-only, sem API key; arquitetura model-agnostic (trocar modelo é config).
- **Plataformas:** desktop primeiro (Windows incluso — o founder desenvolve em Windows; dogfooding onde ninguém testa), mobile como direção futura.

## Convenções

- **Rust stable**, sem nightly. `cargo clippy` e `cargo fmt` limpos como pré-condição de commit.
- Erros com tipos explícitos (`thiserror` na lib, contexto rico no CLI); **nunca** `unwrap()`/`panic!` em caminho de produção da engine.
- Testes: unitários por módulo + testes de crash-recovery (matar o processo no meio da escrita e verificar integridade) + fuzzing dos parsers/formato no CI.
- Commits pequenos e descritivos; inglês no código, identificadores e docs públicas; português nos docs internos de planejamento.
- Releases em ritmo fixo (quinzenal pós-launch), não sob demanda. Feature grande só entra no roadmap se pedida por 2+ usuários.
- Dogfooding obrigatório: o próprio fluxo de trabalho do founder com agentes usa EmbedMind desde a semana 2 do M1.

## Restrições do founder (governam tudo)

- Solo, sem equipe, sem capital externo; dedicação majoritária mas divide com CacheSnap/consultoria.
- **Zero venda ativa:** nada de funil comercial, reuniões de vendas ou outbound. GitHub + conteúdo técnico + self-service.
- Open-source como mecanismo de validação: métricas GitHub (issues de terceiros, PRs externos, downloads recorrentes, lista Pro) substituem entrevistas com clientes.
- SLA público de suporte "best effort" no README — proteger contra burnout de manutenção OSS.

## O que NÃO fazer

- Não adicionar dependência de nuvem, telemetria obrigatória ou qualquer chamada de rede no núcleo — "nada sai da máquina" é auditável no código e é parte do produto.
- Não implementar features premium no núcleo MIT (ver divisão na decisão 3).
- Não quebrar o formato de arquivo sem versionamento + caminho de migração.
- Não expandir escopo do M1 (ver [ROADMAP.md](ROADMAP.md)) — 4 semanas até algo usável vence 12 meses de engine perfeita.
