# RELEASING — publicação no crates.io

Como publicar os crates do EmbedMind no [crates.io](https://crates.io). Cobre a
**ordem obrigatória**, os passos `[MANUAL — founder]`, e um limite do crates.io
que afeta o `embedmind-core` (modelo ONNX embarcado).

> **`cargo publish` real é ato do founder** (guardrail 7 em
> [04-agents.md](04-agents.md); item 1.6 marcado `[MANUAL — founder]` no
> [03-tasks.md](03-tasks.md)). Nenhum agente/CI publica sozinho. Este documento
> prepara tudo até a borda do `cargo publish`; o comando final é digitado por uma
> pessoa, no dia do launch.

## Os três crates

| Crate | Publica como | Papel |
|---|---|---|
| `embedmind-core` | `embedmind-core` | a engine (o ativo) |
| `embedmind-mcp` | `embedmind-mcp` | servidor MCP (casca) |
| `embedmind-cli` | **`embedmind`** | CLI + `embedmind serve` (casca) |

Nomes confirmados **disponíveis** no crates.io em jul/2026 (`embedmind`,
`embedmind-core`, `embedmind-mcp` — nenhum existe ainda no índice).

Metadados de publicação (`description`, `repository`, `homepage`, `keywords`,
`categories`, `readme`, `license = "MIT"`) já estão em cada `Cargo.toml`;
`repository`/`homepage` são herdados de `[workspace.package]`. Cada crate tem seu
próprio `README.md` (o `cargo publish` só empacota arquivos dentro do diretório
do crate — o README da raiz não é alcançável a partir de `crates/*/`).

## Ordem obrigatória: core → mcp → cli

**A ordem não é opcional.** Um crate só pode ser publicado depois que todas as
suas dependências de workspace já estão no índice do crates.io. As dependências
internas carregam `path` **e** `version` em `[workspace.dependencies]`; ao
publicar, o cargo remove o `path` e resolve pela `version` **no índice** — que só
existe após o upstream estar publicado.

```
1. embedmind-core   (sem deps internas)
2. embedmind-mcp    (depende de embedmind-core)
3. embedmind        (depende de embedmind-core e embedmind-mcp)
```

Consequência prática para verificação: `cargo publish --dry-run -p embedmind-mcp`
(e `-p embedmind`) **falha** enquanto o `embedmind-core` não estiver publicado,
com `no matching package named 'embedmind-core' found` — isso é esperado, não é
um defeito de manifesto. Para validar os três localmente **antes** de qualquer
publicação, use `cargo package --workspace` (resolve as deps internas em
conjunto):

```bash
cargo package --workspace          # empacota + compila os 3, na ordem
```

Saída esperada (jul/2026): os 3 pacotes empacotam e compilam sem erro.

## Verificação pré-publicação

```bash
# 1. Suíte verde em todas as plataformas de primeira classe (CI faz isso).
cargo test --workspace

# 2. Lints limpos (pré-condição de commit do projeto).
cargo clippy --workspace --all-targets
cargo fmt --all --check

# 3. Publish-readiness dos 3 crates, resolvendo deps internas em conjunto.
cargo package --workspace

# 4. Dry-run do core isolado (o único que resolve sem upstream publicado).
cargo publish --dry-run -p embedmind-core
```

`cargo publish --dry-run` para `embedmind-mcp` e `embedmind` só fica limpo
**depois** que o `embedmind-core` (e, para o CLI, o `embedmind-mcp`) estiverem
realmente no crates.io — ver seção da ordem acima.

## ⚠️ Limite de tamanho do crates.io (bloqueia `embedmind-core`)

O crates.io impõe um teto de **10 MiB por crate** (pacote comprimido). O
`embedmind-core` embarca o modelo ONNX quantizado + tokenizer via
`include_bytes!` (ADR [0004](adr/0004-modelo-de-embedding-embarcado.md)), então o
pacote fica em **~16 MiB comprimido**:

```
Packaging embedmind-core ...
Packaged 30 files, 23.1MiB (16.2MiB compressed)
```

O `--dry-run` **passa** localmente porque o teto é aplicado **no servidor**, no
upload real — o dry-run não sobe nada. O `cargo publish` real do `embedmind-core`
será **rejeitado** (`crate too large`) enquanto o teto padrão valer.

**Resolução (`[MANUAL — founder]`, antes de publicar o core):** solicitar
aumento de limite ao crates.io. É o procedimento padrão para crates que embarcam
modelos ML; preserva o ADR 0004 (modelo embarcado → `cargo install embedmind`
funciona sem rede, sem passo extra) e a promessa de instalação em um comando.

- Pedir via o formulário/e-mail do crates.io (help@crates.io), citando o crate
  `embedmind-core`, o tamanho (~16 MiB) e a razão (modelo de embedding embarcado,
  local-first, sem download em runtime).
- Só publicar o core **depois** de o aumento ser concedido.

Alternativas **rejeitadas** para caber nos 10 MiB (não fazer sem decisão do
founder — todas contrariam decisões vigentes):

- **Baixar o modelo em build/runtime** — reintroduz rede/dependência externa;
  contraria ADR 0004 e a promessa "nada sai da máquina" / air-gap.
- **`exclude` dos assets + fetch em runtime** — mesmo problema; quebra o
  `cargo install` out-of-the-box.
- **Modelo menor só para caber no teto** — decisão de qualidade (recall/pt-BR),
  fora de escopo aqui; já rastreada como aberta (DESIGN §12, ADR 0004/0010).

> Nota: o teto de **40 MB** do artefato de release (ADR
> [0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md),
> `release.yml`) é um NFR **diferente** — governa o binário comprimido que o
> usuário baixa das Releases, não o pacote do crates.io.

## Passos de publicação (dia do launch — `[MANUAL — founder]`)

Pré-requisitos: aumento de limite do crates.io concedido para `embedmind-core`;
`cargo login` feito; versão final definida (hoje `0.1.0-dev` em
`[workspace.package]` — trocar para a versão de release, ex. `0.1.0`, antes de
publicar).

```bash
cargo publish -p embedmind-core     # 1 — esperar aparecer no índice
cargo publish -p embedmind-mcp      # 2
cargo publish -p embedmind          # 3 (crate publica como "embedmind")
```

Entre passos, aguardar o crate anterior ficar disponível no índice (segundos a
poucos minutos). Cada versão publicada é **imutável** — não dá para republicar a
mesma versão; só é possível `yank`.
