# ADR 0007 — Criptografia reservada no formato desde o dia 1, implementada depois

**Status:** Aceito (jul/2026)

## Contexto

Criptografia at-rest é feature da classe **compliance** (premium, pós-90 dias, tier
enterprise ex-CodeVault). Mas o formato de arquivo é um contrato público que não pode
quebrar — se a criptografia exigir mudança de layout depois, o custo é um
`format_version` bump + migração para todos os usuários.

## Decisão

Reservar no formato v1, sem implementar: bit `encrypted` nos flags do header, campos
`kdf_salt` (16 bytes) e `kdf_params` zerados, e layout de página compatível com cifragem
individual futura (AES-256-GCM por página, nonce derivado de `page_no` + epoch).
Leitores v1 **recusam** arquivos com o bit ligado. Detalhes em [FORMAT.md](../FORMAT.md) §4.

## Alternativas rejeitadas

- **Implementar criptografia já na v0.1:** semanas de trabalho numa feature premium antes
  de validar o núcleo; viola "não implementar features premium no núcleo MIT" e o escopo
  fechado do M1.
- **Não reservar nada ("resolve depois"):** garantiria um format break futuro — viola a
  regra de compatibilidade do formato.

## Consequências

- O módulo premium de compliance pode ser lançado sem quebrar arquivos existentes (upgrade = `embedmind migrate --encrypt`, cópia cifrada).
- Custo presente ≈ 28 bytes reservados no header + um caso a mais no fuzzer (bit ligado → recusa limpa).
- A divisão open-core fica auditável no próprio formato: o núcleo MIT lê tudo que escreve; cifragem é opt-in premium.
