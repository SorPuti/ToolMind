# 🧠 ToolMind

> **Agente autônomo em Rust** que transforma intenções humanas em ações reais no sistema — com acesso ao sistema de arquivos, terminal, visão computacional e automação de interface gráfica.

---

## 📖 Sobre o Projeto

O **ToolMind** é um agente de IA totalmente autônomo construído em Rust, alimentado por modelos de linguagem (LLMs) via streaming. Diferente de assistentes comuns, o ToolMind age: ele executa comandos, lê e escreve arquivos, monitora o sistema, interpreta imagens com OCR e automatiza interfaces gráficas — tudo de forma encadeada, com planejamento rastreável e auditoria de ações.

### ✨ Principais Recursos

| Recurso | Descrição |
|---|---|
| 🔧 **Sistema de Tools** | Ferramentas registradas dinamicamente; o agente decide qual chamar com base no contexto |
| 📋 **Planejamento** | Criação, execução e rastreamento de planos passo a passo (`save_plan`, `update_plan_step`) |
| 📁 **Acesso ao Sistema de Arquivos** | Leitura, escrita, listagem e busca de arquivos com restrições de segurança |
| 💻 **Execução de Comandos** | Execução controlada de binários e comandos no terminal |
| 🖥️ **Monitoramento do Sistema** | Snapshots periódicos de CPU, RAM e disco via `sysinfo` + alertas |
| 👁️ **Visão Computacional** | OCR de imagens com Tesseract CLI (PNG, JPG, BMP, WebP, TIFF) |
| 🖱️ **Automação GUI** *(Windows)* | Captura de tela e automação de teclado/mouse via WinAPI |
| 🔄 **Streaming** | Comunicação em tempo real com a API via SSE (Server-Sent Events) |
| 🛡️ **Segurança** | Caminhos relativos apenas, sem `..`, sem injeção de shell |
| 🗂️ **Auditoria** | Log JSONL de todas as ações (`automation_audit.jsonl`) |

---

## 🚀 Instalação

### Pré-requisitos

| Ferramenta | Versão mínima | Necessário para |
|---|---|---|
| [Rust + Cargo](https://rustup.rs/) | 1.85+ (edition 2024) | Compilar o projeto |
| [Tesseract OCR](https://github.com/tesseract-ocr/tesseract) | 4.x+ | Ferramenta de visão (`vision_context`) |

> **Windows**: os recursos de automação GUI requerem o Windows SDK (geralmente já disponível com o Rust para Windows).

### 1. Clone o Repositório

```bash
git clone https://github.com/SorPuti/ToolMind.git
cd ToolMind
```

### 2. Configure as Variáveis de Ambiente

Crie um arquivo `.env` na raiz do projeto com as suas credenciais da API:

```env
# Chave de API do provedor LLM (ex.: Anthropic, OpenAI, etc.)
API_KEY=sua_chave_aqui

# URL base da API (ex.: https://api.anthropic.com)
API_URL=https://api.seu-provedor.com

# Modelo a ser utilizado (ex.: claude-opus-4-5)
MODEL=nome-do-modelo
```

> ⚠️ **Nunca compartilhe ou comite o arquivo `.env`** — ele já está no `.gitignore`.

### 3. Instale o Tesseract (para OCR)

**Ubuntu / Debian:**
```bash
sudo apt install tesseract-ocr tesseract-ocr-por tesseract-ocr-eng
```

**macOS:**
```bash
brew install tesseract tesseract-lang
```

**Windows:**
Baixe o instalador em: https://github.com/UB-Mannheim/tesseract/wiki

### 4. Compile e Execute

```bash
# Compilar em modo release (recomendado)
cargo build --release

# Executar
./target/release/toolmind
```

Ou, para desenvolvimento:
```bash
cargo run
```

---

## 🗂️ Estrutura do Projeto

```
ToolMind/
├── src/
│   ├── main.rs              # Núcleo do agente: loop de conversação, registro de tools, streaming
│   ├── vision_context.rs    # OCR de imagens via Tesseract CLI
│   └── gui_automation.rs    # Automação de teclado/mouse e captura de tela (Windows)
├── .toolmind/               # Dados de runtime do agente (planos, snapshots, auditoria)
│   ├── current_plan.json    # Plano ativo
│   ├── plan_progress.json   # Progresso das etapas do plano
│   ├── automation_audit.jsonl # Log de auditoria de ações
│   └── snapshots/           # Snapshots do sistema (RAM, CPU, disco)
├── skill_personality.md     # Definição da personalidade e comportamento do agente
├── Cargo.toml               # Manifesto e dependências do projeto
├── .env                     # Configurações locais (não versionado)
└── .gitignore
```

---

## 🛠️ Como Funciona

1. **Inicialização**: O agente carrega as tools disponíveis, lê a `skill_personality.md` e constrói o prompt de sistema.
2. **Entrada do Usuário**: O usuário digita uma instrução no terminal.
3. **Raciocínio**: O LLM decide quais tools chamar e em que ordem.
4. **Execução**: O agente executa as tools localmente (acesso real ao sistema).
5. **Encadeamento**: O resultado de cada tool alimenta a próxima decisão do modelo (até 48 tools por turno).
6. **Resposta**: O agente responde ao usuário com o resultado consolidado.

---

## 🔒 Segurança

- Todos os caminhos de arquivo são **relativos** — acesso absoluto e travessia com `..` são bloqueados.
- Comandos de shell são executados apenas via lista de binários permitidos.
- Resultados de tools são sempre dados reais — o agente não fabrica respostas.

---

## 📄 Licença

Este projeto está licenciado sob termos **restritivos**. Consulte o arquivo [LICENSE](LICENSE) para detalhes completos.

**Resumo**: uso pessoal e educacional é permitido; **uso comercial e uso ilegal são expressamente proibidos**.

---

*Desenvolvido com 🦀 Rust.*
