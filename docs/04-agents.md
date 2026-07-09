# AGENTS.md operacional — EmbedMind

> Documento canônico do pacote SDD (04 de 04). Regras operacionais para agentes de IA
> trabalharem neste repo sem parar para perguntar. Complementa o
> [CLAUDE.md](../CLAUDE.md) da raiz (contexto de produto e decisões-chave — leitura
> obrigatória); em conflito, vale o CLAUDE.md. Os normativos técnicos são
> [DESIGN.md](../DESIGN.md), [FORMAT.md](FORMAT.md) e [docs/adr/](adr/README.md).

## Comandos (copy-paste)

| Ação | Comando |
|---|---|
| Formatar (checagem) | `cargo fmt --all --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Testes (tudo: unit + crash harness + E2E + fuzz smoke) | `cargo test --workspace` |
| Teste de um crate | `cargo test -p embedmind-core` |
| Regenerar corpus de fuzz | `cargo run --example gen_fuzz_corpus` |
| Fuzzing local (Linux, nightly) | `cd fuzz && cargo +nightly fuzz run <alvo>` |
| Build release | `cargo build --release` |
| Rodar o CLI local | `cargo run -p embedmind-cli -- <subcomando>` |

Pré-condição de QUALQUER commit: `cargo fmt --all --check` + clippy `-D warnings` +
`cargo test --workspace` — os três verdes. O CI roda exatamente isso em
Windows/Linux/macOS; não commite o que não passaria na matriz.

## Convenções de código

- **Rust stable** (≥ 1.90, edition 2024). Nunca introduzir dependência de nightly no
  workspace principal (só o workspace `fuzz/` usa nightly).
- Lints de workspace são lei: `unsafe_code = forbid`; `unwrap_used`, `expect_used`,
  `panic` = deny na engine. Erros tipados com `thiserror` na lib; contexto rico no CLI.
- Inglês em código, identificadores, mensagens e docs públicas; português nos docs
  internos de planejamento (DESIGN, ADRs, docs/0X-*.md).
- Commits pequenos e descritivos, no padrão do histórico:
  `área: descrição imperativa` (ex.: `storage: ...`, `mcp: ...`, `cli: ...`,
  `docs: ...`, `ci: ...`), corpo citando item do ROADMAP/ADR quando aplicável.
- Testes acompanham a feature no MESMO commit — crash tests e fuzz targets são parte
  da definição de pronto de qualquer mudança em `storage`/`format`/`index`.

## Guardrails — nunca faça

1. **Nunca** adicionar chamada de rede, telemetria ou dependência de nuvem no núcleo
   (`embedmind-core`) — "nada sai da máquina" é auditável e é parte do produto.
2. **Nunca** quebrar o formato `.mind` sem bump de `format_version` + caminho de
   migração documentado em FORMAT.md (migração é sempre por cópia, nunca in-place).
3. **Nunca** implementar features fora do escopo do núcleo (fronteira no CLAUDE.md
   decisão 3 e no plano §8): time-travel, criptografia, RBAC, auditoria, sync —
   nenhuma entra sem decisão explícita do founder.
4. **Nunca** introduzir tokio/async no workspace (ADR 0009) nem serde no formato
   binário (todo (de)serialize é explícito e fuzzável).
5. **Nunca** deixar lógica de domínio nas cascas (`embedmind-mcp`, `embedmind-cli`) —
   parse → chamada da API core → serialize, nada mais.
6. **Nunca** usar `unwrap`/`expect`/`panic!` em caminho de produção da engine, nem
   alocar a partir de length lido de disco sem validar antes.
7. **Nunca** commitar credenciais, nem publicar (push para público, `cargo publish`
   real, posts) — publicação é ato do founder.
8. **Nunca** expandir escopo além da fase ativa do [03-tasks.md](03-tasks.md) — 4
   semanas até algo usável vence 12 meses de engine perfeita.

## Política de dependências

Orçamento fechado (DESIGN §10): `ort`, `tokenizers`, `xxhash-rust`, `ulid`,
`thiserror`, `clap`, `serde_json` (cascas), `proptest`/`cargo-fuzz` (dev). Adicionar
crate novo exige: (a) justificativa que caberia num ADR, (b) checagem de árvore de
dependências (nada de async runtime ou rede transitiva no core), (c) pin no
`workspace.dependencies`. Na dúvida, implemente as ~200 linhas em vez de importar.

## Desempate em ambiguidade

- Conflito entre docs: **spec ([01-spec.md](01-spec.md)) vale sobre o plano
  ([02-plan.md](02-plan.md))** para comportamento; FORMAT.md vale sobre ambos para
  bytes em disco; CLAUDE.md vale para produto/escopo.
- Crash-safety vence performance; simplicidade vence generalidade; comportamento
  explícito (erro tipado) vence conveniência silenciosa.
- Questão aberta sem default registrado no DESIGN §12 → escolha a opção mais simples
  que preserva o formato de arquivo, registre num ADR novo (numeração 0010+) e siga.
- Um item marcado [MANUAL — founder] no 03-tasks.md apareceu no seu caminho → pare essa
  frente e reporte; não simule a ação humana.

## Loop de trabalho esperado

1. Ler a task (03-tasks.md) e as stories da spec que ela cita; conferir estado real
   com `git log --oneline -10` e `cargo test --workspace`.
2. Implementar dentro do módulo certo (respeitando as camadas do plano §2), com testes
   no mesmo passo — crash tests/fuzz para storage/format/index, E2E para MCP/CLI.
3. Rodar a verificação da task (comando no DoD) + fmt + clippy + suite completa.
4. Atualizar CHANGELOG.md (política de honestidade: regressões e perdas registradas,
   não enterradas) e docs afetados (FORMAT.md se tocou bytes, ADR se decidiu algo).
5. Commit pequeno e descritivo. Não fazer push — o runner/founder decide o destino.
