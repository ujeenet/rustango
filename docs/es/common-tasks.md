# Tareas comunes

Respuestas cortas a «¿cómo hago …?». Cada respuesta ocupa una línea y enlaza a la
página con el código. Las páginas parten de la aplicación de
[Primeros pasos](getting-started.md).

Algunas tareas aún no tienen un ejemplo completo para copiar y ejecutar. Esas
filas enlazan a la página más cercana y dicen «(ejemplo completo en camino)».

## Tabla de contenidos

- [Tareas](#tareas)
- [Más recetas: el recetario](#más-recetas-el-recetario)

## Tareas

| Tarea | Respuesta | Leer |
|---|---|---|
| Registrar un usuario | Comprueba la contraseña con `passwords::strength_score`, hashéala con `passwords::hash` y guarda el hash en la fila del usuario. | [Primeros pasos, Paso 14](getting-started.md#paso-14-añadir-autenticación-jwt) · [Hashear al registrarse](auth-passwords.md#hashear-al-registrarse) (ejemplo completo en camino) |
| Iniciar sesión | Comprueba la contraseña con `passwords::verify` y después abre una sesión o emite un JWT. | [Primeros pasos, Paso 14](getting-started.md#paso-14-añadir-autenticación-jwt) · [Verificar al iniciar sesión](auth-passwords.md#verificar-al-iniciar-sesión) · [Sesiones](auth-sessions.md) (ejemplo completo en camino) |
| Restablecer una contraseña | `PasswordReset::issue` crea un enlace firmado para enviar por correo; `confirm_password_reset_pool` lo comprueba y guarda el nuevo hash. | [Restablecimiento de contraseña](auth-flows.md#restablecimiento-de-contraseña) (ejemplo completo en camino) |
| Subir un archivo | `save_uploads` comprueba el archivo y lo escribe en un `Storage`; tú guardas la clave devuelta en un modelo. | [Subir un avatar de principio a fin](files.md#subir-un-avatar-de-principio-a-fin) |
| Enviar un correo | Construye un `Email`, elige un mailer y llama a `email.send(&mailer)`. | [Enviar correo](email.md#paso-1--construye-un-correo) |
| Ejecutar un trabajo en segundo plano | Implementa `Job`, regístralo en una cola, arranca la cola y llama a `dispatch` desde un handler. | [Trabajos en segundo plano](jobs.md#paso-1--definir-un-trabajo) |
| Probar una vista | Envuelve tu router en un `TestClient` y envía peticiones en el proceso, sin servidor. | [Pruebas](testing.md#paso-1--controla-tu-aplicación-con-testclient) |
| Desplegar | Define `RUSTANGO_ENV=prod`, ejecuta `migrate` y `check --deploy`, y compila con `cargo build --release`. | [Primeros pasos, Paso 18](getting-started.md#paso-18-desplegar-a-producción) · [Auditar antes de desplegar](security.md#auditar-antes-de-desplegar) |

## Más recetas: el recetario

El [recetario](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/cookbook_blog/COOKBOOK.md)
(en inglés) es una aplicación de blog ejecutable con recetas para modelos, el ORM,
migraciones, el admin, APIs REST, multi-tenancy y más. Las recetas están
respaldadas por tests. Para multi-tenancy en sí, empieza por
[Multi-tenancy](tenancy.md).
