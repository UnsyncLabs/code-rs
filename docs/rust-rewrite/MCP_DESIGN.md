# Diseño: servidor MCP de code-rs

Estado: **borrador para revisión**. Nada de esto está implementado todavía.

## 1. Objetivo

Que una IA externa (Claude Code, Claude Desktop, Kiro, cualquier cliente MCP) pueda **ver y
manejar code-rs** como lo haría una persona, y que code-rs use esa misma vía para **proteger
al usuario de extensiones maliciosas**.

Casos de uso que guían el diseño:

1. La IA abre o cierra carpetas y ventanas, abre archivos, lee lo que el usuario ve
   (selección, errores, terminal) y propone cambios.
2. La IA busca, inspecciona, instala, actualiza y desinstala extensiones.
3. **Antes de instalar o actualizar una extensión** (lo pida la IA o el humano), code-rs genera
   un informe de seguridad y la IA conectada lo revisa: de dónde viene, quién la publica, qué
   código trae, qué cambió respecto a la versión anterior.
4. La IA y el usuario se comunican dentro de la ventana: avisos, preguntas, confirmaciones.
5. Encaja con skills de Claude y otros clientes: code-rs publica prompts MCP y una skill
   que explica cómo usar sus herramientas.

**No objetivos (por ahora):** sustituir el chat de VS Code, ejecutar modelos dentro de code-rs,
controlar code-rs desde otra máquina.

## 2. Arquitectura

```
 Cliente MCP (Claude Code, Desktop, Kiro…)
        │  stdio                      │  HTTP streamable (127.0.0.1)
        ▼                             ▼
 code-rs --mcp  ── (puente) ──►  Servidor MCP en el proceso Rust de code-rs
                                   │  ├─ autenticación, permisos, registro de auditoría
                                   │  ├─ herramientas nativas (Rust): ventanas, archivos,
                                   │  │   extensiones, auditoría de seguridad
                                   │  └─ enrutador por ventana
                                   ▼  eventos Tauri
                         Puente MCP en cada ventana (TypeScript, contribución del workbench)
                           └─ editores, comandos, diagnósticos, diálogos de confirmación
```

- **El servidor vive en Rust**, dentro del proceso principal de code-rs: un solo punto de
  control para seguridad y auditoría, y funciona aunque no haya ventanas abiertas.
- **Lo que necesita el workbench** (editores abiertos, selección, ejecutar un comando de VS Code,
  mostrar un diálogo) lo hace una contribución TypeScript en cada ventana. Rust le reenvía la
  petición por eventos Tauri y espera la respuesta.
- **Transportes:**
  - *HTTP streamable* en `127.0.0.1` con puerto aleatorio. Lo usan los clientes que hablan HTTP.
  - *stdio* mediante `code-rs --mcp`: un modo del mismo ejecutable que no abre ventana, lee el
    archivo de descubrimiento y hace de puente hacia la instancia en marcha. Es lo que se
    configura en Claude Code: `{"command": "code-rs", "args": ["--mcp"]}`.
- **Descubrimiento:** al arrancar, code-rs escribe `<app data>/mcp/endpoint.json`
  (`{port, token, pid}`), legible solo por el usuario actual.

## 3. Seguridad del canal

| Riesgo | Medida |
|---|---|
| Otro programa o usuario del equipo se conecta | Token aleatorio de 256 bits por arranque, en `endpoint.json` con permisos solo del usuario; cabecera `Authorization: Bearer` obligatoria |
| Una página web ataca el puerto local (DNS rebinding, CSRF) | Solo escucha en `127.0.0.1`; se rechaza cualquier `Origin` y cualquier `Host` que no sea `127.0.0.1:<puerto>` |
| Acceso desde la red | No hay modo remoto. Nunca se escucha en `0.0.0.0` |
| Servidor encendido sin querer | **Apagado por defecto** (`codeRs.mcp.enabled: false`), igual que la telemetría. Indicador visible en la barra de estado cuando está activo y con clientes conectados |
| Un cliente conectado se queda para siempre | Se ve la lista de clientes en la barra de estado y se puede desconectar uno o regenerar el token |

## 4. Modelo de permisos

Cada herramienta tiene un **nivel de riesgo**. El usuario decide qué hace cada nivel;
estos son los valores por defecto:

| Nivel | Ejemplos | Por defecto |
|---|---|---|
| **0 · Lectura** | listar ventanas, leer archivo, ver diagnósticos, buscar extensiones, ver informe de seguridad | Permitido |
| **1 · Interfaz** | abrir archivo o carpeta en ventana, mostrar aviso, revelar línea | Permitido (se ve en pantalla) |
| **2 · Cambios reversibles** | editar archivos del workspace (con deshacer), crear archivos | **Pregunta**, con diff. Se puede elegir "permitir en esta sesión" |
| **3 · Destructivo o externo** | instalar, actualizar o desinstalar extensiones; borrar archivos; ejecutar en la terminal; escribir fuera del workspace; cerrar ventanas con cambios sin guardar | **Pregunta siempre**. No se puede automatizar por defecto |

Reglas extra:

- **Workspace Trust:** en una carpeta no confiable, el MCP queda en solo lectura (nivel 0).
- **Confirmaciones del lado de code-rs**, nunca del cliente: la IA no puede responderse a sí
  misma. El diálogo muestra qué cliente lo pide, qué herramienta y los detalles (diff,
  informe de la extensión, comando exacto).
- **Ajustes:** `codeRs.mcp.permissions.<nivel>` = `allow` / `ask` / `deny`, y listas por
  herramienta (`codeRs.mcp.tools.deny`).
- **Registro de auditoría:** cada llamada (cliente, herramienta, argumentos resumidos,
  decisión, resultado) en `<app data>/mcp/audit.jsonl` y en el canal de Output "code-rs MCP".
  Nunca sale del equipo.

## 5. Catálogo de herramientas

Nombres con prefijo de grupo. Entre paréntesis, el nivel de riesgo.

**Ventanas y workspace**
- `window_list` (0): ventanas abiertas, carpeta o workspace de cada una, ventana activa.
- `window_open` (1): nueva ventana, opcionalmente con una carpeta.
- `window_close` (1 / 3 si hay cambios sin guardar).
- `workspace_open_folder` (1), `workspace_add_folder` (1), `workspace_close_folder` (1).

**Archivos y editor**
- `file_read` (0), `file_list` (0), `search_text` (0), `search_files` (0): usan los servicios
  Rust que ya existen.
- `editor_state` (0): archivos abiertos, archivo activo, selección y cursor.
- `editor_open` (1): abrir un archivo y revelar un rango.
- `diagnostics_get` (0): errores y avisos (por ejemplo, los de BasedPyright).
- `edit_propose` (2): aplica un `WorkspaceEdit` mostrando el diff; el usuario acepta o rechaza.
- `file_delete` (3).

**Extensiones**
- `extensions_list` (0), `extensions_search` (0): Open VSX.
- `extensions_inspect` (0): manifiesto, publicador y versiones sin instalar.
- `extensions_audit` (0): **informe de seguridad** (sección 6).
- `extensions_install` (3), `extensions_update` (3), `extensions_uninstall` (3): siempre muestran
  el informe en el diálogo de confirmación.

**Comandos y terminal**
- `command_run` (2 o 3): ejecuta un comando de VS Code de una **lista permitida**
  (formatear, organizar imports, guardar…). Los comandos fuera de la lista quedan en nivel 3.
- `terminal_run` (3): ejecuta un comando en la terminal integrada y devuelve la salida.

**Comunicación con el usuario**
- `ui_notify` (1): aviso en la ventana.
- `ui_ask` (1): pregunta al usuario y devuelve la respuesta (sí/no, elegir opción, texto).
- `ui_status` (1): mensaje temporal en la barra de estado ("La IA está revisando la extensión…").

**Recursos MCP (lectura y suscripción)**
- `code-rs://windows`, `code-rs://window/{id}/editor`, `code-rs://diagnostics`,
  `code-rs://extensions`, `code-rs://audit-log`.
- Con suscripción, la IA recibe eventos: archivo guardado, extensión instalada o actualizada,
  ventana abierta o cerrada.

**Prompts MCP**, que aparecen como comandos en el cliente:
- `review-extension` (revisar una extensión antes de instalarla), `explain-diagnostics`,
  `organize-project`.

## 6. Seguridad de extensiones

Es la parte central del proyecto. Se aplica **a cualquier instalación o actualización**, la
pida la IA o el humano desde la vista de Extensiones.

### 6.1 Flujo

1. code-rs (Rust) descarga el VSIX a una carpeta de **cuarentena**, sin registrarlo.
2. Rust analiza el paquete y genera el informe (6.2).
3. Si hay una IA conectada con el permiso `extensions_review`, se le envía el informe por
   notificación MCP y puede añadir su valoración (`extensions_review_submit`).
4. El usuario ve un diálogo con el informe, la valoración de la IA si la hay, y decide.
5. Solo entonces se mueve de cuarentena a la carpeta de extensiones.

**Las actualizaciones automáticas siguen el mismo flujo.** Si el riesgo sube respecto a la
versión instalada (binarios nuevos, dominios nuevos, cambio de publicador), la actualización se
pausa y se pregunta al usuario.

### 6.2 Contenido del informe

| Señal | Cómo se obtiene |
|---|---|
| Origen | URL de descarga, registro (Open VSX), hash SHA-256 del VSIX |
| Publicador | namespace, si está verificado en Open VSX, antigüedad, otras extensiones suyas |
| Popularidad y edad | descargas, valoraciones, fecha de la versión (versión publicada hace minutos = señal de alerta) |
| Firma | verificación de la firma de Open VSX cuando la extensión la publica |
| Código nativo | archivos `.node`, `.exe`, `.dll`, `.so`, `.dylib`, scripts de shell |
| Punto de entrada | `main` / `browser`, eventos de activación (`*` = se activa siempre) |
| Capacidades | uso de `child_process`, `eval` / `new Function`, `fs` fuera del workspace, red (`http`, `fetch`, websockets) |
| Dominios | URLs y hosts que aparecen en el código |
| Ofuscación | código minificado sin source maps, cadenas codificadas en base64 o hex muy largas |
| **Diferencias con la versión anterior** | archivos añadidos o quitados, binarios nuevos, nuevos permisos, nuevos dominios, cambio de publicador |

Resultado: lista de hallazgos con gravedad y un **nivel global** (bajo, medio, alto).

### 6.3 Política

- `codeRs.extensions.audit.mode`: `warn` (por defecto: informa y deja decidir), `block-high`
  (bloquea el riesgo alto salvo confirmación explícita) o `off`.
- **La IA no puede aprobar sola** una extensión de riesgo alto ni de un publicador nuevo:
  recomienda y el humano confirma.
- **Límite honesto:** el análisis estático detecta señales, no demuestra que algo sea seguro.
  El informe lo dice tal cual.

## 7. Comunicación con la IA de cada ventana

- **Del usuario a la IA:** comando "Pedir a la IA…" en la ventana, que envía un mensaje al
  cliente conectado como notificación MCP (`notifications/message`) con el contexto de la
  ventana (archivo, selección).
- **De la IA al usuario:** `ui_notify`, `ui_ask` y `ui_status` en la ventana elegida.
- **Varias IAs a la vez:** cada cliente tiene nombre y color; los diálogos dicen quién pregunta.
- **Ventana de destino:** cada herramienta acepta `windowId`. Si se omite, se usa la ventana
  activa.

## 8. Skills

- Una skill de Claude Code en `packaging/claude-skill/code-rs/SKILL.md`: cómo conectarse,
  qué herramientas hay y buenas prácticas (revisar extensiones antes de instalar, proponer
  cambios en lugar de escribir directamente).
- Los mismos flujos se exponen como **prompts MCP** (sección 5), para que funcionen con
  cualquier cliente, no solo con Claude.

## 9. Fases

| Fase | Contenido |
|---|---|
| **1** | Servidor HTTP y stdio, token, descubrimiento, registro de auditoría, herramientas de nivel 0 y 1 (ventanas, carpetas, lectura, editor, diagnósticos, avisos) |
| **2** | Cuarentena e informe de seguridad (`extensions_audit`), aplicado también a instalaciones manuales; diálogo de confirmación con informe |
| **3** | Herramientas de nivel 2 y 3 con confirmación (`edit_propose`, instalar, desinstalar, `command_run`); revisión por IA (`extensions_review_submit`) |
| **4** | Terminal, suscripciones a eventos, comunicación usuario↔IA, skill y prompts |

## 10. Preguntas abiertas

1. **¿Apagado por defecto?** La propuesta es que sí, por coherencia con la telemetría. Lo
   activaría el usuario en Ajustes o al conectar el primer cliente.
2. **Revisión de extensiones sin IA conectada:** ¿basta el análisis estático de Rust o
   quieres que se pueda usar una IA configurada de forma permanente (clave de API) para
   revisarlas siempre?
3. **Lista permitida de `command_run`:** ¿empezamos con una lista corta (formatear, organizar
   imports, guardar, abrir vista previa) y la ampliamos?
4. **`terminal_run`:** ¿lo incluimos en la primera versión pública o lo dejamos para después
   por el riesgo?
5. **Telemetría del MCP:** propuesta: ninguna. El registro de auditoría es solo local.
