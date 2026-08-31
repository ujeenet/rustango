# Contraseñas

Almacenar una contraseña significa almacenar algo que un atacante no puede
revertir ni siquiera con toda tu base de datos en la mano. **Rustango** te ofrece
eso en dos llamadas — `hash` a la entrada, `verify` a la salida — respaldadas por
**argon2id**, el ganador *memory-hard* de la Password Hashing Competition y la
primera opción actual de OWASP. Nunca almacenas, registras ni comparas el texto
plano.

[![Contraseñas en Rustango: hash() produce una cadena PHC argon2id con sal, verify() comprueba un intento contra ella, y verify_dummy() iguala los tiempos de inicio de sesión](../img/auth-passwords.png)](../img/auth-passwords.png)

> **Fuente:** `rustango::passwords` (`hash`, `verify`, `verify_dummy`,
> `strength_score`, `StrengthIssue`) — detrás de la característica `passwords`
> (activada por defecto). Para las utilidades de contraseña de usuario integradas
> con la multitenencia, consulta `rustango::tenancy::password`.
>
> **Versión ejecutable:** cada fragmento a continuación está copiado del ejemplo
> probado [`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/tests/auth_passwords.rs)
> — `cargo test -p auth_demo --test auth_passwords`.

> **¿Algún término aquí es nuevo para ti?** *hash*, *sal*, *argon2id*, *cadena
> PHC* — consulta el [glosario](glossary.md).

> Esta es la inmersión profunda de la sección «Hashear y verificar contraseñas»
> de la [guía de seguridad](security.md).

---

## Tabla de contenidos
- [Inicio rápido](#quick-start) · [Por qué argon2id](#why-argon2id)
- [Hashear al registrarse](#hashing-on-signup) · [Verificar al iniciar sesión](#verifying-on-login)
- [Inicios de sesión con tiempos seguros](#timing-safe-logins-account-enumeration) · [Comprobaciones de robustez](#strength-checks)
- [Dónde vive el hash](#where-the-hash-lives) · [Notas y límites](#notes-and-limits)

---

## Inicio rápido

```rust
use rustango::passwords::{hash, verify};

// Signup — store the returned PHC string, never the plaintext.
let stored: String = hash("CorrectHorseBatteryStaple!42")?;

// Login — check an attempt against the stored hash.
if verify("CorrectHorseBatteryStaple!42", &stored)? {
    // credentials good
}
```

`hash` devuelve una [cadena PHC](https://github.com/P-H-C/phc-string-format) — una
línea autodescriptiva que lleva el algoritmo, sus parámetros de coste, la sal
aleatoria y el digest:

```text
$argon2id$v=19$m=19456,t=2,p=1$<base64 salt>$<base64 hash>
```

Como la sal y los parámetros viajan *dentro* de la cadena, `verify` solo necesita
el valor almacenado y el intento — no hay una columna de sal separada que
gestionar.

---

## Por qué argon2id

`hash` usa **argon2id** con los valores por defecto recomendados por OWASP (m=19
MiB, t=2, p=1). argon2id es *memory-hard*: cada intento cuesta RAM real, que es lo
que embota las granjas de GPU/ASIC que hacen que los hashes rápidos (MD5,
SHA-256, incluso bcrypt a bajo coste) sean vulnerables a la fuerza bruta. Dos
propiedades importan para la corrección:

- **El salado es automático y por hash.** Hashear la misma contraseña dos veces
  produce dos cadenas PHC diferentes, de modo que contraseñas idénticas no
  colisionan en tu tabla y los ataques con tablas arcoíris precalculadas no
  aplican.

  ```rust
  let a = hash("same-password-12345")?;
  let b = hash("same-password-12345")?;
  assert_ne!(a, b);                 // different random salt each time
  assert!(verify("same-password-12345", &a)?);
  assert!(verify("same-password-12345", &b)?);
  ```

- **La verificación es de tiempo constante** en la comparación del digest (el
  propio `PasswordVerifier` de argon2), de modo que una fuga de tiempo byte a
  byte no puede revelar qué parte de un intento era correcta.

---

## Hashear al registrarse

```rust
use rustango::passwords::{hash, strength_score};

fn create_user(username: &str, plaintext: &str) -> Result<String, String> {
    // Optional: nudge users away from weak choices (see below).
    let issues = strength_score(plaintext);
    if !issues.is_empty() {
        return Err(format!("password too weak: {issues:?}"));
    }
    // Store the PHC string on the user row (e.g. auth_users.password_hash).
    hash(plaintext).map_err(|e| e.to_string())
}
```

---

## Verificar al iniciar sesión

```rust
use rustango::passwords::verify;

// `stored` is the PHC string you saved at signup.
let ok = verify(attempt, &stored)?;
```

`verify` devuelve:
- `Ok(true)` — el intento coincide.
- `Ok(false)` — no coincide.
- `Err(PasswordError::Verify)` — `stored` no era una cadena PHC válida (una
  columna corrupta o truncada), así que trátalo como un inicio de sesión fallido,
  no como un 500.

---

## Inicios de sesión con tiempos seguros (enumeración de cuentas)

Si tu inicio de sesión ejecuta el costoso `verify` **solo** cuando el nombre de
usuario existe, un nombre de usuario desconocido responde notablemente más rápido
que uno real — y esa brecha de tiempo permite a un atacante enumerar las cuentas
válidas. `verify_dummy` la cierra: llámalo en la rama de usuario-no-encontrado (y
cuenta-inactiva) para que cada inicio de sesión invierta el trabajo de un `verify`
de argon2 sin importar el caso.

```rust
use rustango::passwords::{verify, verify_dummy};

let row = users::find_by_username(username).await?;
let authenticated = match row {
    Some(u) if u.is_active => verify(attempt, &u.password_hash)?,
    _ => {
        verify_dummy(attempt); // burn the same work, then fail
        false
    }
};
```

---

## Comprobaciones de robustez

`strength_score` devuelve un `Vec<StrengthIssue>` — vacío significa «lo bastante
bueno». Es una heurística intencionadamente ligera para *animar* a los usuarios,
no una barrera de política estricta; combínala con una comprobación contra listas
de filtraciones (HIBP / pwned-passwords) para despliegues serios.

```rust
use rustango::passwords::{strength_score, StrengthIssue};

assert!(strength_score("Tr0ub4dor&3-CorrectBattery").is_empty());
assert!(strength_score("password123").contains(&StrengthIssue::KnownWeak));
assert!(strength_score("short").contains(&StrengthIssue::TooShort));
```

| `StrengthIssue` | Se activa cuando |
|---|---|
| `TooShort` | menos de 12 caracteres |
| `NoDigitsOrSymbols` | solo letras — sin dígito ni símbolo |
| `NoVariety` | solo letras minúsculas |
| `KnownWeak` | coincide con la pequeña lista integrada de contraseñas débiles (sin distinguir mayúsculas y minúsculas) |

---

## Dónde vive el hash

La cadena PHC no es más que una columna `String` en el modelo de cuenta que sea
tuyo. En el ejemplo
[`auth_demo`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/auth_demo/src/models.rs):

```rust
#[derive(Model, Clone, Debug)]
#[rustango(table = "auth_users", display = "username")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 150, unique)]
    pub username: String,
    #[rustango(max_length = 254)]
    pub email: String,
    #[rustango(max_length = 255)]      // PHC strings are ~95 chars at these params
    pub password_hash: String,
    pub is_active: bool,
    pub is_superuser: bool,
}
```

Una vez que el usuario está autenticado, pasa el testigo a una
[sesión](auth-sessions.md) (para aplicaciones de navegador) o emite un
[JWT](auth-jwt.md) (para APIs).

---

## Notas y límites

- **Nunca** almacenes, registres ni compares con `==` el texto plano. `hash` →
  almacenar; `verify` → comprobar. Ese es todo el contrato.
- **Los parámetros de coste son los valores por defecto de OWASP**, integrados de
  serie. Son un suelo razonable; elevarlos más adelante es seguro — los hashes
  antiguos siguen verificándose (sus parámetros viven en la cadena PHC), y puedes
  volver a hashear en el siguiente inicio de sesión con éxito para actualizarlos.
- `strength_score` es una heurística, no un motor de políticas — no detectará
  `Summer2024!`. Superpón una búsqueda en listas de filtraciones para una
  aplicación real de la robustez.
- Para aplicaciones multitenencia con el almacén de usuarios del framework,
  prefiere `rustango::tenancy::password` (el mismo argon2id, integrado con el
  modelo de usuario del inquilino). Este módulo es la versión autónoma para
  aplicaciones que poseen su propia tabla User.


---

## Véase también

- [Sesiones](auth-sessions.md)
- [Flujos de cuenta](auth-flows.md)
- [Backends de autenticación](auth-backends.md)
- [Guía de seguridad](security.md)
