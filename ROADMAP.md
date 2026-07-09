# ROADMAP — EmbedMind

Plano técnico derivado do plano de construção open-core de jul/2026 (`plano_construcao_open_core_2026_07.html`). Princípios: lançar antes de estar orgulhoso · release a cada 2–3 semanas · todo marco termina em algo público · um único projeto em janela de lançamento ativo por vez.

## Visão geral das fases

```
Semana 0        M1 (sem. 1–4)      M2 (sem. 5–8)       M3 (sem. 9–12)      Pós-90 dias
README battle → núcleo mínimo   → lançamento público → grafo + funil Pro → M4–M6 (se GO)
(gate: decisão)  (repo privado)    (dia 35, hard stop)  (go/no-go dia 90)   premium + vitrine
```

> **Nota (atualizada em 07/jul/2026):** o satélite de calibração **AgentLock foi removido do caminho** por decisão do founder — o M1 começa imediatamente após a semana 0, e o lançamento do EmbedMind é a estreia da máquina de lançamento. Consequência assumida: os erros de primeira vez (post, Show HN, cadência de resposta) serão queimados no próprio EmbedMind; mitigação: preparar o material de launch (post, GIF, FAQ) com antecedência dentro do M1, não na véspera do dia 35.
>
> **Linha do tempo concreta (M1 iniciando em 07/jul/2026):** dia 35 (launch público, hard stop) ≈ **11/ago** · alarme "repo ainda privado" no dia 45 ≈ **21/ago** · go/no-go do dia 90 ≈ **05/out**.

---

## Semana 0 — Gate de decisão (README-driven development)

- [x] Escrever o README impecável do EmbedMind (este repo) — pitch, GIF imaginado, tabela free/premium.
- [x] Compará-lo com o README do Enclave (desafiante, mesma barreira técnica de sistemas).
- [x] **Decisão da aposta principal ao fim da semana 0.** Critério de desempate: *qual você usaria todo dia*.
- [ ] Agendar no calendário: data-limite de launch público (dia 35 do M1 ≈ 11/ago/2026), revisão de go/no-go (dia 90 ≈ 05/out/2026).

**Gate: ✅ RESOLVIDO em 07/jul/2026 — EmbedMind venceu.** O Enclave volta ao banco de ideias; este roadmap é o plano ativo dos 90 dias. (Se o Enclave vencesse, este roadmap seria arquivado.)

---

## M1 — Semanas 1–4: o núcleo mínimo que já impressiona

Repo **privado** até o fim do marco. Prioridade absoluta: crash-safety antes de features.

| # | Entrega | Depende de | Status |
|---|---|---|---|
| 1.1 | Formato de arquivo único + **WAL/crash-safety** básica | — (fundação de tudo) | ✅ |
| 1.2 | KV store + API Rust limpa (`embedmind-core`) | 1.1 | ✅ |
| 1.3 | Busca vetorial **HNSW** in-file + embeddings ONNX embarcados (CPU) | 1.1, 1.2 | ✅ (incl. chunking de memórias longas) |
| 1.4 | Servidor MCP: `remember` / `recall` / `forget` | 1.2, 1.3 | ✅ (ADR 0009: stdio direto, sem SDK) |
| 1.5 | Memória automática de contexto de projeto | 1.4 | ✅ (raiz git / `.embedmind.toml`) |
| 1.6 | Instalação em 1 comando (`cargo install` + binários); testado com Claude Code **+ 1 outro agente** | 1.4 | 🔶 CLI completo (`serve` = servidor MCP) + pipeline de release por tag `v*` (3 plataformas, teto de tamanho); faltam `cargo publish` (A2) + teste manual com 2 agentes |
| 1.7 | README final: GIF de demo em 30s + **benchmark honesto** vs. sqlite-vec e zvec | 1.3, 1.6 | ⬜ |
| 1.8 | Testes de crash-recovery + fuzzing do formato no CI | 1.1 | ✅ |

**🎯 Milestone:** v0.1 funcional de ponta a ponta, **dogfooding diário do founder a partir da semana 2**.

---

## M2 — Semanas 5–8: lançamento público e primeiro ciclo de feedback

| # | Entrega | Depende de |
|---|---|---|
| 2.1 | **Dia 35: repo público + lançamento coordenado** — Show HN, r/ClaudeAI, r/LocalLLaMA, r/rust, X. Post: *"I built persistent memory for coding agents in Rust — single file, no server"* | M1 completo |
| 2.2 | Toda issue respondida em <24h (capacidade de resposta É o marketing nesta fase) | 2.1 |
| 2.3 | **Full-text search** na engine | 1.2 |
| 2.4 | **Filtros de metadados** no `recall` | 2.3 |
| 2.5 | **Bindings Python** (destrava LangChain/agentes custom — multiplicador de audiência) | API da engine estável (M1) |
| 2.6 | 2º post técnico: a engine por dentro (WAL, HNSW em arquivo único) | 2.1 |
| 2.7 | Releases quinzenais guiados pelas issues mais pedidas | 2.2 |

**🎯 Milestone:** projeto vivo em público, ciclo de release estabelecido, primeiros usuários externos reais.

---

## M3 — Semanas 9–12: aprofundar o núcleo + instrumentar o funil premium

| # | Entrega | Depende de |
|---|---|---|
| 3.1 | **Camada de grafo simples** (entidades + relações entre memórias) — o diferencial vs. "só vetor" que nenhum embarcado tem completo | 2.3, 2.4 |
| 3.2 | **Proveniência básica** por memória (qual agente/sessão gravou) — grátis, semente da rastreabilidade premium | 1.4 |
| 3.3 | Página **"Pro/Team — coming soon"** com lista premium (histórico, compliance, rastreabilidade, integrações) + captura de e-mail — *o instrumento de validação de receita* | 2.1 |
| 3.4 | 3º post: caso de uso real com números ("30 dias usando memória persistente no meu fluxo com agentes") | dogfooding contínuo |
| 3.5 | **Avaliação go/no-go do dia 90** | 3.3 + métricas abaixo |

**🎯 Milestone:** núcleo diferenciado (vetor + texto + grafo), funil premium instrumentado, dados para a decisão de 90 dias.

### Métricas do go/no-go (dia 90, ~7 semanas pós-launch)

| Métrica | 🔴 Fraco | 🟡 Bom | 🟢 Forte | O que mede |
|---|---|---|---|---|
| Estrelas | < 300 | 300–1.500 | > 1.500 | Ressonância da mensagem (vaidade útil) |
| Issues/discussões de terceiros | < 10 | 10–40 | > 40 | **Uso real** |
| PRs externos aceitos | 0 | 1–5 | > 5 | Comunidade nascendo |
| Downloads recorrentes/semana | < 100 | 100–1.000 | > 1.000 | Retenção |
| E-mails lista Pro + perguntas comerciais | 0–2 | 3–15 | > 15 | **Intenção de receita** |

**Regras de decisão (compromisso prévio):**
- **2+ colunas 🟢** (sendo uma delas *issues* ou *lista Pro*) → **GO**: iniciar o 1º módulo premium (M4).
- **Maioria 🟡** → mais 90 dias no núcleo OSS com *um* reposicionamento de mensagem.
- **Maioria 🔴** com launch bem executado → **reempacotar** (mesma engine, outra porta de entrada). Só após 2 empacotamentos fracos a tese se considera refutada.
- **Cláusula anti-armadilha-do-construtor:** repo ainda privado no dia 45 = alarme vermelho; lançar o que existir.

---

## Pós-90 dias (M4–M6, condicionado a GO)

| Frente | Conteúdo | Depende de |
|---|---|---|
| **1º módulo premium** | O mais pedido na lista Pro — provavelmente sync/equipe ou criptografia; se houver demanda regulada: tier **Enterprise ex-CodeVault** (RBAC, air-gap, auditoria, compliance LGPD/BACEN) | 3.3 (sinal da lista), 3.2 |
| **Vitrine da engine** | App pequeno de notas/memória por voz 100% local (Chefe de Gabinete em miniatura) — demonstra a engine ao público não-dev, testa 2ª fonte de receita | Engine estável, 3.1 |
| **Bindings adicionais** | TypeScript; Swift/C conforme demanda | 2.5 |
| **Licença comercial de embarque** | Modelo SQLite/Realm: US$ 2–20k/ano por produto embarcante | Engine como crate/lib independente |
| **Segunda aposta** | Avaliar tirar o **Paperjet** do banco | GO confirmado + apetite |
| **Ingestão de código como fonte de memória** | Indexar localmente (Tree-sitter, sem LLM externo) código/docs do projeto como memórias além das gravadas via `remember` — mesma promessa "nada sai da máquina", mas cobrindo conhecimento *já existente*, não só o acumulado em sessão. Avaliar após GO; risco de virar concorrência direta de players como o Graphify (YC, tração grande) em vez de nicho adjacente | Engine estável, 3.1 (grafo) |

**Fallback de demanda** (se as apostas frustrarem): CLI de relatório de desperdício de tokens (fatia mínima da Torre de Controle) — valida em 1 semana.

---

## Grafo de dependências (funcionalidades)

```
WAL/arquivo único (1.1)
 ├── KV + API Rust (1.2)
 │    ├── HNSW/vetor (1.3) ──┐
 │    └── Full-text (2.3)    ├── MCP remember/recall/forget (1.4)
 │         └── Filtros (2.4) ┘    ├── Contexto de projeto (1.5)
 │              └── Grafo (3.1)   └── Proveniência básica (3.2)
 │                                      └── Rastreabilidade premium (M4+)
 ├── Fuzzing/crash tests (1.8) — contínuo
 └── Criptografia at-rest (M4+) ── RBAC/auditoria/air-gap (Enterprise)
API estável ── Bindings Python (2.5) ── Bindings TS/Swift (M4+)
```

## Riscos que moldam a sequência

- **Commoditização** (sqlite-vec/LanceDB/zvec com times pagos) → por isso M1 = 4 semanas até usável, e o posicionamento é "memória para agentes", não "database para RAG".
- **MCP perder relevância** → engine em camadas; a casca MCP é substituível.
- **Corrupção de dados** (mata o moat de confiabilidade) → WAL antes de features, fuzzing no CI, honestidade brutal no changelog.
- **Estrelas sem receita** → página Pro instrumentada já no M3; a decisão do dia 90 é forçada.
- **Burnout OSS** → SLA "best effort" público, releases em ritmo fixo, feature grande só com 2+ pedidos.
