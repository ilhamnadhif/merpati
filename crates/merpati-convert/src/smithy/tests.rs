use merpati_core::model::{BodyMode, Collection, Item, RequestItem};
use merpati_core::{AuthType, HttpRequest};
use serde_json::{Value, json};

use super::*;

const WEATHER: &str = include_str!("../../testdata/weather.smithy");
const WEATHER_JSON: &str = include_str!("../../testdata/weather.smithy.json");

fn req<'a>(c: &'a Collection, name: &str) -> &'a RequestItem {
    c.requests().into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no request `{name}`"))
}

fn http<'a>(c: &'a Collection, name: &str) -> &'a HttpRequest {
    req(c, name).http().unwrap()
}

fn body_json(r: &HttpRequest) -> Value {
    serde_json::from_str(&r.body.raw).unwrap_or_else(|e| panic!("{e}: {}", r.body.raw))
}

fn header<'a>(r: &'a HttpRequest, name: &str) -> Option<&'a merpati_core::model::KeyValue> {
    r.headers.iter().find(|h| h.key.eq_ignore_ascii_case(name))
}

fn errors(text: &str) -> Vec<Diagnostic> {
    check(text).diagnostics.into_iter().filter(Diagnostic::is_error).collect()
}

fn syntax_error(text: &str) -> Diagnostic {
    match parse_idl(text) {
        Ok(_) => panic!("expected a syntax error"),
        Err(d) => d,
    }
}

/// A comparable dump of a collection (ids removed).
fn outline(c: &Collection) -> Vec<String> {
    fn walk(items: &[Item], depth: usize, out: &mut Vec<String>) {
        for it in items {
            match it {
                Item::Folder(f) => {
                    out.push(format!("{}[{}] {}", "  ".repeat(depth), f.name, f.description));
                    walk(&f.items, depth + 1, out);
                }
                Item::Request(r) => {
                    let h = r.http().unwrap();
                    out.push(format!("{}{} {} {} | {}", "  ".repeat(depth), r.name, h.method, h.url, r.description));
                    for p in &h.path_vars {
                        out.push(format!("  path {}={} ({})", p.key, p.value, p.description));
                    }
                    for p in &h.params {
                        out.push(format!("  query {}={} {}", p.key, p.value, p.enabled));
                    }
                    for p in &h.headers {
                        out.push(format!("  header {}={} {}", p.key, p.value, p.enabled));
                    }
                    out.push(format!("  body {:?} {}", h.body.mode, h.body.raw));
                    for e in &r.examples {
                        out.push(format!("  example {} {} {}", e.name, e.status, e.body));
                    }
                }
            }
        }
    }
    let mut out = vec![format!("{} | {} | {:?}", c.name, c.description, c.auth)];
    out.extend(c.variables.iter().map(|v| format!("var {}={}", v.key, v.value)));
    walk(&c.items, 0, &mut out);
    out
}

// -- parsing ---------------------------------------------------------------

#[test]
fn weather_parses_without_findings() {
    let checked = check(WEATHER);
    assert!(checked.model.is_some());
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    assert_eq!(checked.summary(), "Smithy 2.0: 1 service, 5 operations");
}

#[test]
fn weather_model_shape() {
    let m = parse(WEATHER).unwrap();
    assert_eq!(m.version, "2.0");
    assert_eq!(m.namespace, "example.weather");
    assert_eq!(m.metadata["authors"], json!(["Merpati tests"]));
    assert_eq!(m.metadata["suppressions"][0]["id"], "HttpMethodSemantics");

    let svc = m.shape("example.weather#Weather").unwrap();
    assert_eq!(svc.kind, ShapeKind::Service);
    assert_eq!(svc.version.as_deref(), Some("2006-03-01"));
    assert_eq!(svc.docs(), Some("Provides weather forecasts for cities."));
    assert!(svc.traits.contains_key("aws.protocols#restJson1"));
    assert_eq!(svc.get_trait("paginated").unwrap()["pageSize"], "pageSize");
    assert_eq!(svc.pos, Pos::new(17, 9));

    let city = m.shape("example.weather#City").unwrap();
    assert_eq!(city.identifiers["cityId"].id, "example.weather#CityId");
    let lifecycle: Vec<&str> = city.lifecycle.keys().map(String::as_str).collect();
    assert_eq!(lifecycle, ["create", "read", "list"]);

    // Inline input with target elision from the resource.
    let input = m.shape("example.weather#GetCityInput").unwrap();
    assert!(input.has_trait("input"));
    let id = &input.members["cityId"];
    assert_eq!(id.target, "example.weather#CityId");
    assert!(id.has_trait("required") && id.has_trait("httpLabel"));
    let out = m.shape("example.weather#GetCityOutput").unwrap();
    assert_eq!(out.members["coordinates"].target, "example.weather#CityCoordinates");
    assert_eq!(out.members["name"].target, "smithy.api#String");

    // Mixin members come first; defaults and jsonName are kept.
    let create = m.shape("example.weather#CreateCityInput").unwrap();
    let names: Vec<&str> = create.members.keys().map(String::as_str).collect();
    assert_eq!(names, ["clientToken", "name", "coordinates", "climate", "tags", "foundedAt"]);
    assert_eq!(create.members["clientToken"].docs(), Some("Makes retries safe."));
    assert_eq!(create.members["climate"].get_trait("default"), Some(&json!("TEMPERATE")));
    assert_eq!(create.members["climate"].get_trait("jsonName"), Some(&json!("climate_zone")));
    assert!(!create.has_trait("mixin"));

    // Enums.
    let climate = m.shape("example.weather#Climate").unwrap();
    assert_eq!(climate.members["TROPICAL"].get_trait("enumValue"), Some(&json!("tropical")));
    assert_eq!(climate.members["POLAR"].get_trait("enumValue"), Some(&json!("POLAR")));
    let units = m.shape("example.weather#Units").unwrap();
    assert_eq!(units.members["IMPERIAL"].get_trait("enumValue"), Some(&json!(2)));

    // Apply statements, text blocks.
    let time = m.shape("example.weather#GetCurrentTime").unwrap();
    assert_eq!(time.docs(), Some("Returns the current time of the service.\n  Indented line stays indented.\n"));
    let coords = m.shape("example.weather#CityCoordinates").unwrap();
    assert_eq!(coords.members["latitude"].get_trait("range"), Some(&json!({"min": -90, "max": 90})));

    // Traits with structured, list and string values.
    let get = m.shape("example.weather#GetCity").unwrap();
    assert_eq!(get.get_trait("http"), Some(&json!({"method": "GET", "uri": "/cities/{cityId}"})));
    assert_eq!(get.get_trait("examples").unwrap()[1]["error"]["shapeId"], "NoSuchResource");
    assert_eq!(get.errors[0].id, "example.weather#NoSuchResource");
    let summary = m.shape("example.weather#CitySummary").unwrap();
    assert_eq!(summary.get_trait("references"), Some(&json!([{"resource": "City"}])));
    assert_eq!(m.shape("example.weather#CityId").unwrap().get_trait("pattern"), Some(&json!("^[A-Za-z0-9 ]+$")));
}

#[test]
fn json_ast_matches_idl() {
    let checked = check(WEATHER_JSON);
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    let idl = parse(WEATHER).unwrap();
    let ast = parse(WEATHER_JSON).unwrap();
    assert_eq!(ast.version, "2.0");
    assert_eq!(ast.namespace, "example.weather");
    let mut a: Vec<&String> = idl.shapes.keys().collect();
    let mut b: Vec<&String> = ast.shapes.keys().collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
    for (id, s) in &idl.shapes {
        let t = &ast.shapes[id];
        assert_eq!(s.kind, t.kind, "{id}");
        let ma: Vec<(&String, &String)> = s.members.iter().map(|(k, m)| (k, &m.target)).collect();
        let mb: Vec<(&String, &String)> = t.members.iter().map(|(k, m)| (k, &m.target)).collect();
        assert_eq!(ma, mb, "{id}");
        for (k, m) in &s.members {
            let mut x: Vec<&String> = m.traits.keys().collect();
            let mut y: Vec<&String> = t.members[k].traits.keys().collect();
            x.sort();
            y.sort();
            assert_eq!(x, y, "{id}${k}");
        }
    }
    assert_eq!(outline(&idl.to_collection().0), outline(&ast.to_collection().0));
}

#[test]
fn json_ast_positions_point_at_shape_keys() {
    let m = parse_json_ast(WEATHER_JSON).unwrap();
    let s = m.shape("example.weather#GetCity").unwrap();
    let line = WEATHER_JSON.lines().position(|l| l.contains("\"example.weather#GetCity\":")).unwrap() + 1;
    assert_eq!(s.pos.line as usize, line);
    assert_eq!(s.pos.column, 9);
}

// -- conversion -------------------------------------------------------------

#[test]
fn weather_collection_tree() {
    let (c, warnings) = import(WEATHER).unwrap();
    assert_eq!(c.name, "Weather Service");
    assert!(c.description.starts_with("Provides weather forecasts for cities."));
    assert!(c.description.contains("Protocol: restJson1"));
    assert_eq!(c.auth.kind, AuthType::Bearer);
    assert_eq!(c.auth.get("token"), "{{bearerToken}}");
    let vars: Vec<&str> = c.variables.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(vars, ["baseUrl", "bearerToken"]);
    assert_eq!(warnings, ["Smithy models do not define endpoints: set the `baseUrl` variable"]);

    // Root: operation bound to the service, then the resource folder.
    assert_eq!(c.items.len(), 2);
    assert_eq!(c.items[0].name(), "GetCurrentTime");
    let Item::Folder(city) = &c.items[1] else { panic!("City folder") };
    assert_eq!(city.name, "City");
    assert_eq!(city.description, "A city and its forecasts.\n\nIdentifiers: `cityId` (CityId).");
    let names: Vec<&str> = city.items.iter().map(Item::name).collect();
    assert_eq!(names, ["CreateCity", "GetCity", "ListCities", "Forecast"]);
    let Item::Folder(forecast) = &city.items[3] else { panic!("Forecast folder") };
    assert_eq!(forecast.items[0].name(), "GetForecast");
    assert_eq!(c.request_count(), 5);
}

#[test]
fn weather_get_city_uses_labels_and_examples() {
    let (c, _) = import(WEATHER).unwrap();
    let r = req(&c, "GetCity");
    let h = r.http().unwrap();
    assert_eq!(h.method, "GET");
    assert_eq!(h.url, "{{baseUrl}}/cities/:cityId");
    assert_eq!(h.path_vars.len(), 1);
    assert_eq!(h.path_vars[0].key, "cityId");
    assert_eq!(h.path_vars[0].value, "seattle");
    assert_eq!(h.body.mode, BodyMode::None);
    assert_eq!(r.description, "Gets a city by id.\n\nRead-only. Errors: NoSuchResource (404).");
    assert_eq!(r.examples.len(), 2);
    assert_eq!(r.examples[0].name, "Seattle");
    assert_eq!(r.examples[0].status, 200);
    let body: Value = serde_json::from_str(&r.examples[0].body).unwrap();
    assert_eq!(body, json!({"name": "Seattle", "coordinates": {"latitude": 47.6, "longitude": -122.3}}));
    assert_eq!(r.examples[1].status, 404);
    assert_eq!(r.examples[1].body.replace([' ', '\n'], ""), "{\"resourceType\":\"City\"}");
    let ex_req = r.examples[1].request.as_ref().unwrap();
    assert_eq!(ex_req.path_vars[0].value, "atlantis");
}

#[test]
fn weather_create_city_body_headers_and_errors() {
    let (c, _) = import(WEATHER).unwrap();
    let r = req(&c, "CreateCity");
    let h = r.http().unwrap();
    assert_eq!(h.method, "PUT");
    assert_eq!(h.url, "{{baseUrl}}/cities");
    let token = header(h, "X-Client-Token").unwrap();
    assert_eq!(token.value, "{{$guid}}");
    assert!(token.enabled);
    assert_eq!(token.description, "Makes retries safe.");
    assert_eq!(header(h, "Content-Type").unwrap().value, "application/json");
    assert_eq!(
        body_json(h),
        json!({
            "name": "string",
            "coordinates": {"latitude": -90.0, "longitude": 0.0},
            "climate_zone": "TEMPERATE",
            "tags": {"key": "string"},
            "foundedAt": "2024-01-01T00:00:00Z"
        })
    );
    assert!(r.description.contains("Idempotent."));
    assert!(r.description.contains("Errors: NoSuchResource (404), ServiceUnavailable (500)."));
    let ex: Vec<(String, u16)> = r.examples.iter().map(|e| (e.name.clone(), e.status)).collect();
    assert_eq!(
        ex,
        [("201 Created".to_string(), 201), ("NoSuchResource".into(), 404), ("ServiceUnavailable".into(), 500)]
    );
    assert_eq!(serde_json::from_str::<Value>(&r.examples[0].body).unwrap(), json!({"cityId": "string"}));
}

#[test]
fn weather_query_params_and_timestamps() {
    let (c, _) = import(WEATHER).unwrap();
    let list = http(&c, "ListCities");
    assert_eq!(list.url, "{{baseUrl}}/cities");
    let params: Vec<(&str, &str, bool)> =
        list.params.iter().map(|p| (p.key.as_str(), p.value.as_str(), p.enabled)).collect();
    assert_eq!(params, [("nextToken", "string", false), ("pageSize", "1", false)]);
    assert_eq!(
        req(&c, "ListCities").description,
        "Read-only. Paginated: inputToken `nextToken`, outputToken `nextToken`, items `items`, pageSize `pageSize`."
    );

    let fc = http(&c, "GetForecast");
    assert_eq!(fc.url, "{{baseUrl}}/cities/:cityId/forecast");
    assert_eq!(fc.params[0].key, "units");
    assert_eq!(fc.params[0].value, "1");

    // restJson1 bodies use epoch-seconds by default.
    let time = req(&c, "GetCurrentTime");
    assert_eq!(serde_json::from_str::<Value>(&time.examples[0].body).unwrap(), json!({"time": 1704067200}));
    assert_eq!(
        time.description,
        "Returns the current time of the service.\n  Indented line stays indented.\n\nRead-only."
    );

    let fc_item = req(&c, "GetForecast");
    let body: Value = serde_json::from_str(&fc_item.examples[0].body).unwrap();
    assert_eq!(body, json!({"chanceOfRain": 0.0, "precipitation": {"rain": 0.0}}));
}

const NOTES: &str = r#"
$version: "2.0"
namespace com.example.notes

use aws.protocols#restJson1

@restJson1
@httpApiKeyAuth(name: "X-Api-Key", in: "header")
service Notes {
    version: "2024-05-01"
    operations: [PutAttachment, UpdateNote, GetNote, DeleteNote, Ping]
}

@http(method: "PUT", uri: "/notes/{noteId}/attachments/{path+}")
operation PutAttachment {
    input := {
        @required @httpLabel noteId: String
        @required @httpLabel path: String
        @httpHeader("Content-Type") contentType: String
        @httpPayload data: Attachment
    }
}

@mediaType("image/png")
blob Attachment

@http(method: "PATCH", uri: "/notes/{noteId}?mode=merge", code: 200)
operation UpdateNote {
    input := {
        @required @httpLabel noteId: String
        @httpPrefixHeaders("X-Meta-") meta: StringMap
        @required @httpPayload note: Note
    }
    output := {
        @httpHeader("ETag") etag: String
        @httpPayload note: Note
    }
    errors: [NotFound]
}

@http(method: "GET", uri: "/notes/{noteId}")
@auth([])
@readonly
operation GetNote {
    input := {
        @required @httpLabel noteId: String
        @httpQuery("fields") fields: FieldList
        @httpQueryParams extra: StringMap
        @httpHeader("If-Modified-Since") since: Timestamp
    }
    output := {
        @required @httpPayload text: String
    }
}

@http(method: "DELETE", uri: "/notes/{noteId}", code: 204)
@deprecated(message: "Use archive instead")
operation DeleteNote {
    input := { @required @httpLabel noteId: String }
}

@http(method: "GET", uri: "/ping")
operation Ping {}

structure Note {
    @required
    @jsonName("note_title")
    title: String
    body: Document
    tags: Tags
    @timestampFormat("http-date")
    updated: Timestamp
}

list FieldList { member: String }
set Tags { member: String }
map StringMap { key: String, value: String }

@error("client")
@httpError(404)
structure NotFound { message: String }
"#;

#[test]
fn rest_json_payloads_auth_and_prefix_headers() {
    let checked = check(NOTES);
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    let (c, _) = import(NOTES).unwrap();
    assert_eq!(c.name, "Notes");
    assert_eq!(c.auth.kind, AuthType::ApiKey);
    assert_eq!(c.auth.get("key"), "X-Api-Key");
    assert_eq!(c.auth.get("value"), "{{apiKey}}");
    assert!(c.variables.iter().any(|v| v.key == "apiKey" && v.is_secret()));
    let names: Vec<&str> = c.items.iter().map(Item::name).collect();
    assert_eq!(names, ["PutAttachment", "UpdateNote", "GetNote", "DeleteNote", "Ping"]);

    // Greedy label, blob payload with @mediaType and an explicit Content-Type member.
    let put = http(&c, "PutAttachment");
    assert_eq!(put.url, "{{baseUrl}}/notes/:noteId/attachments/:path");
    assert_eq!(put.body.mode, BodyMode::Binary);
    let ct: Vec<&str> =
        put.headers.iter().filter(|h| h.key.eq_ignore_ascii_case("content-type")).map(|h| h.value.as_str()).collect();
    assert_eq!(ct, ["image/png"]);

    // Structure payload, literal query, prefix headers, jsonName, timestamp formats.
    let upd = http(&c, "UpdateNote");
    assert_eq!(upd.url, "{{baseUrl}}/notes/:noteId?mode=merge");
    assert_eq!(upd.params[0].key, "mode");
    assert_eq!(upd.params[0].value, "merge");
    let meta = header(upd, "X-Meta-name").unwrap();
    assert!(!meta.enabled);
    assert_eq!(
        body_json(upd),
        json!({"note_title": "string", "body": {}, "tags": ["string"], "updated": "Mon, 01 Jan 2024 00:00:00 GMT"})
    );
    let upd_item = req(&c, "UpdateNote");
    assert_eq!(upd_item.examples[0].headers[0].key, "ETag");
    assert_eq!(upd_item.examples[1].status, 404);

    // Query list, query params map, header timestamp, operation without auth, string payload output.
    let get_item = req(&c, "GetNote");
    let get = get_item.http().unwrap();
    assert_eq!(get.auth.kind, AuthType::None);
    let keys: Vec<&str> = get.params.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["fields", "key"]);
    assert_eq!(header(get, "If-Modified-Since").unwrap().value, "Mon, 01 Jan 2024 00:00:00 GMT");
    assert_eq!(get_item.examples[0].body, "string");
    assert_eq!(get_item.examples[0].language.as_deref(), Some("text"));

    let del = req(&c, "DeleteNote");
    assert!(del.description.starts_with("Deprecated: Use archive instead"));
    assert_eq!(del.examples[0].status, 204);
    assert!(del.examples[0].body.is_empty());

    let ping = http(&c, "Ping");
    assert_eq!(ping.url, "{{baseUrl}}/ping");
    assert!(ping.body.raw.is_empty());
}

#[test]
fn aws_json_rpc_operations() {
    let text = r#"$version: "2"
namespace com.amazonaws.queue
use aws.protocols#awsJson1_0
use aws.auth#sigv4

@awsJson1_0
@sigv4(name: "queue")
service Queue { version: "2012-11-05", operations: [SendMessage] }

operation SendMessage {
    input := { @required QueueUrl: String, DelaySeconds: Integer }
    output := { MessageId: String }
}
"#;
    let checked = check(text);
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    let (c, _) = import(text).unwrap();
    assert_eq!(c.auth.kind, AuthType::AwsV4);
    assert_eq!(c.auth.get("service"), "queue");
    let r = http(&c, "SendMessage");
    assert_eq!(r.method, "POST");
    assert_eq!(r.url, "{{baseUrl}}/");
    assert_eq!(header(r, "X-Amz-Target").unwrap().value, "Queue.SendMessage");
    assert_eq!(header(r, "Content-Type").unwrap().value, "application/x-amz-json-1.0");
    assert_eq!(body_json(r), json!({"QueueUrl": "string", "DelaySeconds": 0}));
}

#[test]
fn rest_xml_bodies() {
    let text = r#"$version: "2"
namespace example.xml
use aws.protocols#restXml
@restXml
service Store { operations: [PutItem] }
@http(method: "POST", uri: "/items")
operation PutItem {
    input := {
        @xmlName("ItemName") name: String
        count: Integer
    }
}
"#;
    let (c, _) = import(text).unwrap();
    let r = http(&c, "PutItem");
    assert_eq!(header(r, "Content-Type").unwrap().value, "application/xml");
    assert_eq!(r.body.raw, "<PutItemInput>\n  <ItemName>string</ItemName>\n  <count>0</count>\n</PutItemInput>");
}

#[test]
fn models_without_service_import_http_operations() {
    let text = r#"$version: "2"
namespace example.loose
@http(method: "POST", uri: "/echo")
operation Echo { input := { message: String } }
operation NoHttp {}
"#;
    let (c, warnings) = import(text).unwrap();
    assert_eq!(c.name, "example.loose");
    assert_eq!(c.request_count(), 1);
    assert_eq!(body_json(http(&c, "Echo")), json!({"message": "string"}));
    assert!(warnings.iter().any(|w| w.contains("no service")));
}

#[test]
fn several_services_get_a_folder_each() {
    let text = r#"$version: "2"
namespace example.multi
service A { operations: [Op1] }
service B { operations: [Op2] }
@http(method: "GET", uri: "/one") operation Op1 {}
@http(method: "GET", uri: "/two") operation Op2 {}
"#;
    let (c, _) = import(text).unwrap();
    assert_eq!(c.name, "example.multi");
    let names: Vec<&str> = c.items.iter().map(Item::name).collect();
    assert_eq!(names, ["A", "B"]);
    assert_eq!(c.request_count(), 2);
}

#[test]
fn idl_v1_constructs() {
    let text = r#"$version: "1.0"
namespace example.v1

service Legacy { version: "1", operations: [GetThing] }

@http(method: "GET", uri: "/things/{id}")
@readonly
operation GetThing {
    input: GetThingInput,
    output: GetThingOutput,
}

structure GetThingInput {
    @required
    @httpLabel
    id: String,
}

structure GetThingOutput {
    color: Color,
    sizes: Sizes,
}

@enum([{ value: "red", name: "RED" }, { value: "blue", name: "BLUE" }])
string Color

set Sizes { member: Integer }
"#;
    let checked = check(text);
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    assert_eq!(checked.model.as_ref().unwrap().version, "1.0");
    let (c, _) = import(text).unwrap();
    let r = req(&c, "GetThing");
    assert_eq!(r.http().unwrap().url, "{{baseUrl}}/things/:id");
    let body: Value = serde_json::from_str(&r.examples[0].body).unwrap();
    assert_eq!(body, json!({"color": "red", "sizes": [0]}));
}

// -- syntax errors -------------------------------------------------------------

#[test]
fn syntax_errors_have_positions() {
    let d = syntax_error("$version: \"2\"\nnamespace a.b\n\nstructure Foo {\n    bar String\n}\n");
    assert_eq!(d.pos, Pos::new(5, 9));
    assert_eq!(d.message, "expected `:` and a target after member `bar`, found `String`");
    assert_eq!(d.to_string(), "5:9: error: expected `:` and a target after member `bar`, found `String`");

    let d = syntax_error("namespace a.b\nstring Name\n@documentation(\"unterminated)\nstring X\n");
    assert_eq!(d.pos, Pos::new(3, 16));
    assert_eq!(d.message, "unterminated string");

    let d = syntax_error("namespace a.b\n\nwidget Foo {}\n");
    assert_eq!((d.pos, d.message.as_str()), (Pos::new(3, 1), "unknown shape type `widget`"));

    let d = syntax_error("namespace a.b\n@http(method: \"GET\" uri: \"/\"\nstructure X {}");
    assert_eq!(d.pos, Pos::new(3, 11));
    assert!(d.message.starts_with("expected `:`"), "{}", d.message);

    let d = syntax_error("$version: \"2\"\nstructure Foo {}\n");
    assert_eq!((d.pos, d.message.as_str()), (Pos::new(2, 1), "expected `namespace` before shape definitions"));

    let d = syntax_error("namespace a.b\nuse Foo\n");
    assert_eq!(d.pos, Pos::new(2, 5));

    let d = syntax_error("namespace a.b\nstructure A {\n  x: String\n");
    assert_eq!(d.pos, Pos::new(4, 1));
    assert!(d.message.contains("end of file"), "{}", d.message);

    let d = syntax_error("namespace a.b\nintEnum E { A }\n");
    assert_eq!(d.pos, Pos::new(2, 13));

    let d = syntax_error("$version: \"3\"\n");
    assert_eq!(d.message, "unsupported Smithy IDL version `3`");

    let d = syntax_error("namespace a.b\nstring S\nstring S2 with [S] # oops\n");
    assert_eq!((d.pos, d.message.as_str()), (Pos::new(3, 20), "unexpected character `#`"));
}

#[test]
fn json_ast_syntax_errors() {
    let d = parse("{\n  \"smithy\": \"2.0\",\n  \"shapes\": { \"a#B\": { \"type\": \"structure\" }, }\n}").unwrap_err();
    assert_eq!(d.pos.line, 3);
    assert!(d.message.starts_with("invalid JSON"), "{}", d.message);
    let d = parse(r#"{"smithy": "2.0", "shapes": {"a#B": {"type": "widget"}}}"#).unwrap_err();
    assert!(d.message.contains("unknown type `widget`"));
    let d = parse(r#"{"smithy": "2.0", "shapes": {"B": {"type": "string"}}}"#).unwrap_err();
    assert!(d.message.contains("must be absolute"));
}

#[test]
fn strings_escapes_and_text_blocks() {
    let text = "namespace a.b\n@documentation(\"tab\\tquote\\\" \\u00e9 \\ud83d\\ude00\")\nstring A\n\
                @documentation(\"\"\"\n    Hello\n      world \\\"x\\\"\n    \"\"\")\nstring B\n\
                /// First line\n///Second line\n@title(\"T\")\nstring C\n";
    let m = parse_idl(text).unwrap();
    assert_eq!(m.shape("a.b#A").unwrap().docs(), Some("tab\tquote\" é 😀"));
    assert_eq!(m.shape("a.b#B").unwrap().docs(), Some("Hello\n  world \"x\"\n"));
    assert_eq!(m.shape("a.b#C").unwrap().docs(), Some("First line\nSecond line"));
}

// -- validation -----------------------------------------------------------------

#[test]
fn validation_reports_unknown_targets() {
    let text = r#"$version: "2"
namespace example.bad
service S { operations: [Op, Missing] }
@http(method: "POST", uri: "/x")
operation Op {
    input := { a: Strin }
    errors: [Oops]
}
"#;
    let errs = errors(text);
    let msgs: Vec<String> = errs.iter().map(ToString::to_string).collect();
    assert_eq!(
        msgs,
        [
            "3:30: error: unknown operation `Missing`",
            "6:16: error: unknown target `Strin` for member `a`",
            "7:14: error: unknown error `Oops`",
        ]
    );
}

#[test]
fn validation_of_http_bindings() {
    let text = r#"$version: "2"
namespace example.bad
use aws.protocols#restJson1
@restJson1
service S { operations: [NoHttp, Labels, Payload] }

operation NoHttp {}

@http(method: "GET", uri: "/items/{id}/{missing}")
operation Labels {
    input := {
        @httpLabel id: String
        @required @httpLabel extra: String
    }
}

@http(method: "POST", uri: "/payload")
operation Payload {
    input := {
        @httpPayload a: String
        b: String
    }
}
"#;
    let msgs: Vec<String> = errors(text).iter().map(ToString::to_string).collect();
    assert_eq!(
        msgs,
        [
            "7:11: error: operation `NoHttp` is bound to service `S` (restJson1) but has no `@http` trait",
            "9:1: error: URI label `{missing}` of `Labels` has no matching `@httpLabel` input member",
            "12:9: error: `@httpLabel` member `id` must be marked `@required`",
            "13:19: error: `@httpLabel` member `extra` has no `{extra}` label in the URI `/items/{id}/{missing}`",
            "17:1: error: `Payload` uses `@httpPayload`, so every other input member must be bound to the URI, \
             query or headers (unbound: b)",
        ]
    );
    // Conversion still works: operations without @http are skipped with a warning.
    let (c, warnings) = import(text).unwrap();
    assert_eq!(c.request_count(), 2);
    assert!(warnings.iter().any(|w| w.contains("`NoHttp` has no `@http` trait; skipped")));
    assert!(warnings.iter().any(|w| w.starts_with("7:11: error:")));
}

#[test]
fn validation_of_kinds_mixins_and_traits() {
    let text = r#"$version: "2"
namespace example.bad
service S { operations: [NotAnOp] }
structure NotAnOp {}
structure Base {}
structure Child with [Base] {}
@restJson1
service T {}
@http(method: "GET", uri: "no-slash")
operation Bad {}
@http(method: "GET", uri: "/a/{b}x")
operation Bad2 {}
"#;
    let all: Vec<String> = check(text).diagnostics.iter().map(ToString::to_string).collect();
    assert_eq!(
        all,
        [
            "3:26: error: operation `NotAnOp` must be a operation, not a structure",
            "6:23: error: `Base` is not a mixin (missing `@mixin`)",
            "7:1: warning: unknown trait `@restJson1`; add `use aws.protocols#restJson1`",
            "9:1: error: URI `no-slash` must start with `/`",
            "11:1: error: label in `{b}x` must span a whole path segment in URI `/a/{b}x`",
        ]
    );
}

#[test]
fn duplicate_routes_and_shapes() {
    let text = r#"$version: "2"
namespace example.dup
service S { operations: [A, B] }
@http(method: "GET", uri: "/x/{id}") operation A { input := { @required @httpLabel id: String } }
@http(method: "GET", uri: "/x/{key}") operation B { input := { @required @httpLabel key: String } }
string A
"#;
    let msgs: Vec<String> = errors(text).iter().map(ToString::to_string).collect();
    assert_eq!(
        msgs,
        ["5:1: error: `GET /x/{key}` of `B` conflicts with operation `A`", "6:8: error: shape `A` is defined twice"]
    );
}

#[test]
fn query_literals_disambiguate_routes_and_library_traits_are_known() {
    let text = r#"$version: "2"
namespace example.s3
use aws.protocols#restXml
@restXml
@aws.api#service(sdkId: "S3")
service S { operations: [GetA, GetB] }
@http(method: "GET", uri: "/{Bucket}?x-id=GetA") operation GetA { input := { @required @httpLabel Bucket: String } }
@http(method: "GET", uri: "/{Bucket}?x-id=GetB") @smithy.rules#staticContextParams({}) @aws.iam#iamAction(name: "x")
operation GetB { input := { @required @httpLabel Bucket: String } }
"#;
    let checked = check(text);
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);
    let (c, _) = import(text).unwrap();
    assert_eq!(http(&c, "GetB").url, "{{baseUrl}}/:Bucket?x-id=GetB");
}

#[test]
fn elision_without_source_is_an_error() {
    let text = "namespace a.b\nstructure S { $id }\n";
    let msgs: Vec<String> = errors(text).iter().map(ToString::to_string).collect();
    assert_eq!(
        msgs,
        ["2:15: error: cannot elide the target of member `$id`: the shape has no mixins and no `for` resource"]
    );
}

// -- detection --------------------------------------------------------------------

#[test]
fn detection() {
    assert!(looks_like_smithy(WEATHER));
    assert!(looks_like_smithy(WEATHER_JSON));
    assert!(looks_like_smithy("// comment\nnamespace a.b\n\nstructure X {}"));
    assert!(!looks_like_smithy("openapi: 3.0.0\npaths: {}"));
    assert!(!looks_like_smithy("{\"openapi\": \"3.0.0\"}"));
    assert!(!looks_like_smithy("namespace is a word in prose"));
    assert_eq!(crate::detect(WEATHER), Some(crate::ImportFormat::Smithy));
    assert_eq!(crate::detect(WEATHER_JSON), Some(crate::ImportFormat::Smithy));
    let out = crate::import_auto(WEATHER).unwrap();
    assert_eq!(out.collections[0].request_count(), 5);
}

#[test]
fn import_fails_only_on_syntax_errors() {
    match import("namespace a.b\nstructure {") {
        Err(crate::ConvertError::Parse(msg)) => assert_eq!(msg, "2:11: error: expected a shape name, found `{`"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn checked_summary_counts() {
    let c = check("namespace a.b\nstructure X { y: Nope }");
    assert_eq!(c.summary(), "Smithy: 0 services, 0 operations, 1 error");
    let c = check("namespace a.b\nstructure {");
    assert_eq!(c.summary(), "2:11: error: expected a shape name, found `{`");
    assert!(c.model.is_none());
}

// -- multi-file models ----------------------------------------------------------

const SHOP: [(&str, &str); 5] = [
    ("main.smithy", include_str!("../../testdata/smithy-multi/main.smithy")),
    ("operations.smithy", include_str!("../../testdata/smithy-multi/operations.smithy")),
    ("common.smithy", include_str!("../../testdata/smithy-multi/common.smithy")),
    ("traits.smithy", include_str!("../../testdata/smithy-multi/traits.smithy")),
    ("item.json", include_str!("../../testdata/smithy-multi/item.json")),
];

fn all(checked: &Checked) -> Vec<String> {
    checked.diagnostics.iter().map(ToString::to_string).collect()
}

#[test]
fn multi_file_model_resolves_across_files() {
    let checked = check_files(&SHOP);
    assert!(checked.diagnostics.is_empty(), "{:#?}", all(&checked));
    assert_eq!(checked.summary(), "Smithy 2.0: 1 service, 3 operations in 5 files");
    let m = checked.model.unwrap();
    assert_eq!(m.namespace, "example.shop");
    assert_eq!(m.files, SHOP.map(|(n, _)| n.to_string()));

    // Same namespace in another file, `use` of another namespace, JSON AST shapes.
    let svc = m.shape("example.shop#Shop").unwrap();
    assert_eq!(svc.errors[0].id, "example.common#ServiceError");
    let get = m.shape("example.shop#GetItem").unwrap();
    assert_eq!(get.pos, Pos { file: 1, line: 11, column: 11 });
    assert_eq!(m.file_name(get.pos), Some("operations.smithy"));
    assert_eq!(m.shape("example.shop#GetItemInput").unwrap().members["itemId"].target, "example.shop#ItemId");
    assert_eq!(m.shape("example.shop#GetItemOutput").unwrap().members["item"].target, "example.shop#Item");
    let item = m.shape("example.shop#Item").unwrap();
    assert_eq!(item.pos.file, 4);
    assert_eq!(item.members["price"].target, "example.common#Money");
    let put = m.shape("example.shop#PutPriceInput").unwrap();
    assert_eq!(put.members["price"].target, "example.common#Money");
    // A trait defined in another file and imported with `use`.
    assert_eq!(m.shape("example.shop#PutPrice").unwrap().get_trait("example.common#audited").unwrap()["level"], "high");

    // Mixin of another namespace; `apply` from another file, also to an inherited member.
    let list = m.shape("example.shop#ListItemsInput").unwrap();
    let names: Vec<&str> = list.members.keys().map(String::as_str).collect();
    assert_eq!(names, ["nextToken", "pageSize"]);
    assert_eq!(list.members["pageSize"].get_trait("range"), Some(&json!({"min": 1, "max": 100})));
    assert_eq!(list.members["pageSize"].trait_position("range"), Pos { file: 3, line: 11, column: 31 });
    assert_eq!(get.docs(), Some("Gets an item by id."));
    assert_eq!(item.members["name"].get_trait("length"), Some(&json!({"min": 1, "max": 80})));

    // Metadata: arrays are concatenated in file order, equal values kept once.
    assert_eq!(m.metadata["authors"], json!(["Merpati tests", "Common types team", "JSON AST export"]));
    assert_eq!(m.metadata["team"], json!({"name": "shop", "oncall": "shop-oncall"}));
}

#[test]
fn multi_file_service_converts_to_a_collection() {
    let (c, warnings) = import_files(&SHOP).unwrap();
    assert_eq!(warnings, ["Smithy models do not define endpoints: set the `baseUrl` variable"]);
    assert_eq!(c.name, "Shop Service");
    assert!(c.description.starts_with("A small shop split over several files."), "{}", c.description);
    assert_eq!(c.auth.kind, AuthType::Bearer);
    let names: Vec<&str> = c.items.iter().map(Item::name).collect();
    assert_eq!(names, ["GetItem", "ListItems", "PutPrice"]);

    let get = req(&c, "GetItem");
    assert_eq!(get.description, "Gets an item by id.\n\nRead-only. Errors: NotFound (404).");
    let h = get.http().unwrap();
    assert_eq!((h.method.as_str(), h.url.as_str()), ("GET", "{{baseUrl}}/items/:itemId"));
    let ok: Value = serde_json::from_str(&get.examples[0].body).unwrap();
    assert_eq!(ok, json!({"item": {"id": "string", "name": "string", "price": {"amount": 0, "currency": "string"}}}));

    let list = http(&c, "ListItems");
    let params: Vec<(&str, &str)> = list.params.iter().map(|p| (p.key.as_str(), p.value.as_str())).collect();
    assert_eq!(params, [("nextToken", "string"), ("pageSize", "1")]);

    let put = http(&c, "PutPrice");
    assert_eq!((put.method.as_str(), put.url.as_str()), ("PUT", "{{baseUrl}}/items/:itemId/price"));
    assert_eq!(body_json(put), json!({"price": {"amount": 0, "currency": "string"}}));
}

#[test]
fn a_single_file_of_a_split_model_has_unknown_targets() {
    // What the Spec Hub reported before files were assembled.
    let msgs: Vec<String> = errors(SHOP[1].1).iter().map(ToString::to_string).collect();
    assert!(msgs.contains(&"15:9: error: unknown target `ItemId` for member `itemId`".to_string()), "{msgs:#?}");
    assert!(!msgs.iter().any(|m| m.contains("operations.smithy")), "single texts have no file names");
}

#[test]
fn diagnostics_carry_the_file_name() {
    let broken = SHOP[1].1.replace("uri: \"/items/{itemId}\"", "uri: \"/items/{id}\"");
    let mut files = SHOP.to_vec();
    files[1].1 = &broken;
    let checked = check_files(&files);
    assert_eq!(
        all(&checked),
        [
            "operations.smithy:10:1: error: URI label `{id}` of `GetItem` has no matching `@httpLabel` input member",
            "operations.smithy:14:9: error: `@httpLabel` member `itemId` has no `{itemId}` label in the URI \
             `/items/{id}`",
        ]
    );
    assert_eq!(checked.in_file("operations.smithy").count(), 2);
    assert_eq!(checked.in_file("main.smithy").count(), 0);
    assert_eq!(checked.diagnostics[0].pos, Pos { file: 1, line: 10, column: 1 });
    // Import keeps working; warnings name the file.
    let (_, warnings) = import_files(&files).unwrap();
    assert!(warnings[0].starts_with("operations.smithy:10:1: error: URI label"), "{warnings:#?}");
}

#[test]
fn duplicate_shapes_across_files() {
    let a = "$version: \"2\"\nnamespace ex.dup\n\nstructure Thing {\n    a: String\n}\n\n@tags([\"a\"])\nstring Name\n";
    let b = "$version: \"2\"\nnamespace ex.dup\n\nstructure Thing { b: Integer }\n\n@tags([\"b\"])\nstring Name\n";
    let checked = check_files(&[("a.smithy", a), ("b.smithy", b)]);
    assert_eq!(
        all(&checked),
        ["b.smithy:4:11: error: shape `ex.dup#Thing` is defined twice: the definition at a.smithy:4:11 is different"]
    );
    // The same shape in two files is one shape; list traits are concatenated.
    let m = checked.model.unwrap();
    assert_eq!(m.shape("ex.dup#Name").unwrap().get_trait("tags"), Some(&json!(["a", "b"])));
    assert_eq!(m.shape("ex.dup#Thing").unwrap().members.keys().collect::<Vec<_>>(), ["a"]);

    // Conflicting trait values of an identical shape.
    let c = "namespace ex.dup\n@documentation(\"one\")\nstring Id\n";
    let d = "namespace ex.dup\n\n@documentation(\"two\")\nstring Id\n";
    let msgs = all(&check_files(&[("c.smithy", c), ("d.smithy", d)]));
    assert_eq!(
        msgs,
        ["d.smithy:3:1: error: conflicting values for trait `@documentation` on `Id` (also set at c.smithy:2:1)"]
    );
    // Same file: unchanged message.
    let e = "namespace ex.dup\nstring Id\nstring Id\n";
    assert_eq!(all(&check_files(&[("e.smithy", e)])), ["e.smithy:3:8: error: shape `Id` is defined twice"]);
}

#[test]
fn shape_ids_are_unique_ignoring_case() {
    let a = "namespace ex.case\nstring UserId\n";
    let b = "namespace ex.case\n\nstring Userid\n";
    assert_eq!(
        all(&check_files(&[("a.smithy", a), ("b.smithy", b)])),
        ["b.smithy:3:8: error: shape `ex.case#Userid` conflicts with `ex.case#UserId` defined at a.smithy:2:8: \
          shape ids must differ in more than case"]
    );
    // Other namespaces are other ids.
    let c = "namespace ex.other\nstring userid\n";
    assert!(all(&check_files(&[("a.smithy", a), ("c.smithy", c)])).is_empty());
}

#[test]
fn smithy_build_paths() {
    let json = r#"{"version": "1.0", "sources": ["model", "extra/a.smithy"], "imports": ["model", "shared"],
                   "plugins": {"openapi": {}}}"#;
    assert_eq!(build_config_paths(json).unwrap(), ["model", "extra/a.smithy", "shared"]);
    assert_eq!(build_config_paths(r#"{"version": "1.0"}"#).unwrap(), Vec::<String>::new());
    assert!(build_config_paths(r#"{"sources": "model"}"#).unwrap_err().contains("must be an array"));
    assert!(build_config_paths("{").unwrap_err().contains("invalid smithy-build.json"));
}

#[test]
fn apply_and_metadata_conflicts_across_files() {
    let a = "metadata owner = \"a\"\nmetadata tags = [\"x\"]\nnamespace ex.m\n@documentation(\"Doc\")\n@tags([\"t1\"])\n\
             string S\n";
    let b = "metadata owner = \"b\"\nmetadata tags = [\"y\"]\nnamespace ex.m\napply S @tags([\"t2\"])\n\
             apply S @documentation(\"Doc\")\napply S @documentation(\"Other\")\napply Missing @documentation(\"x\")\n";
    let checked = check_files(&[("a.smithy", a), ("b.smithy", b)]);
    assert_eq!(
        all(&checked),
        [
            "b.smithy:1:10: error: metadata `owner` conflicts with the value defined at a.smithy:1:10",
            "b.smithy:6:9: error: conflicting values for trait `@documentation` on `S` (also set at a.smithy:4:1)",
            "b.smithy:7:7: error: cannot apply traits to unknown shape `ex.m#Missing`",
        ]
    );
    let m = checked.model.unwrap();
    assert_eq!(m.metadata["owner"], "a");
    assert_eq!(m.metadata["tags"], json!(["x", "y"]));
    assert_eq!(m.shape("ex.m#S").unwrap().get_trait("tags"), Some(&json!(["t1", "t2"])));
    assert_eq!(m.shape("ex.m#S").unwrap().docs(), Some("Doc"));
}

#[test]
fn syntax_errors_in_any_file_fail_the_model() {
    let checked = check_files(&[("good.smithy", WEATHER), ("bad.smithy", "namespace a.b\nstructure {")]);
    assert!(checked.model.is_none());
    assert_eq!(all(&checked), ["bad.smithy:2:11: error: expected a shape name, found `{`"]);
    assert_eq!(checked.summary(), "bad.smithy:2:11: error: expected a shape name, found `{`");
    match import_files(&[("bad.json", "{\"smithy\": \"2.0\", \"shapes\": {\"B\": {\"type\": \"string\"}}}")]) {
        Err(crate::ConvertError::Parse(msg)) => {
            assert!(msg.starts_with("bad.json:1:30: error: shape id `B` must be absolute"), "{msg}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn single_file_functions_match_multi_file_ones() {
    let one = check(WEATHER);
    let many = check_files(&[("", WEATHER)]);
    assert_eq!(one.summary(), many.summary());
    let (a, b) = (parse(WEATHER).unwrap(), parse_files(&[("", WEATHER)]).unwrap());
    assert_eq!((a.shapes, a.metadata, a.parse_diagnostics), (b.shapes, b.metadata, b.parse_diagnostics));
    let (a, _) = import(WEATHER).unwrap();
    let (b, _) = import_files(&[("weather.smithy", WEATHER)]).unwrap();
    assert_eq!(outline(&a), outline(&b));
}

#[test]
fn related_files_follow_references() {
    // Same namespace and shape names as the shop, but nothing in common.
    let unrelated = "$version: \"2\"\nnamespace example.shop\nservice Shop { operations: [] }\nstructure NotFound {}\n";
    let meta = "metadata suppressions = []\n";
    let broken = "namespace example.shop\nstructure {";
    let mut named: Vec<(&str, &str)> = SHOP.to_vec();
    named.extend([("other.smithy", unrelated), ("meta.smithy", meta), ("broken.smithy", broken)]);
    let sources: Vec<SourceFile> = named.iter().map(|(n, t)| SourceFile::parse(*n, t)).collect();
    let refs: Vec<&SourceFile> = sources.iter().collect();
    // From any file of the shop model, in either direction of the references.
    for start in 0..5 {
        assert_eq!(related_files(&refs, start), [0, 1, 2, 3, 4, 6], "start {start}");
    }
    assert_eq!(related_files(&refs, 5), [5, 6]);
    assert_eq!(related_files(&refs, 7), [7]);
    assert_eq!(
        sources[7].syntax_error().map(ToString::to_string).unwrap(),
        "broken.smithy:2:11: error: expected a shape name, found `{`"
    );
    assert_eq!(sources[0].namespace(), Some("example.shop"));

    // Scanning a folder: other specs are skipped, broken Smithy files are kept.
    assert!(SourceFile::detect("openapi.json", "{\"openapi\": \"3.0.0\"}").is_none());
    assert!(SourceFile::detect("broken.json", "{ \"smithy\": ").is_none());
    assert!(SourceFile::detect("notes.txt", "just prose").is_none());
    assert!(SourceFile::detect("item.json", SHOP[4].1).is_some_and(|f| f.syntax_error().is_none()));
    assert!(SourceFile::detect("bad.idl", broken).is_some_and(|f| f.syntax_error().is_some()));

    let set: Vec<&SourceFile> = related_files(&refs, 1).into_iter().map(|i| refs[i]).collect();
    let checked = check_sources(&set);
    assert!(checked.diagnostics.is_empty(), "{:#?}", all(&checked));
    assert_eq!(import_sources(&set).unwrap().0.request_count(), 3);
}

/// The official restJson1 protocol tests (`smithy-aws-protocol-tests`, 31
/// files) as one model. Not part of the repository: run with
/// `MERPATI_SMITHY_PROTOCOL_TESTS=<dir> cargo test -p merpati-convert -- --ignored`.
#[test]
#[ignore]
fn protocol_test_files_form_one_model() {
    let dir = std::env::var("MERPATI_SMITHY_PROTOCOL_TESTS").expect("MERPATI_SMITHY_PROTOCOL_TESTS");
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "smithy"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&p).unwrap()))
        .collect();
    files.sort();
    assert_eq!(files.len(), 31);
    // Every file on its own still parses.
    for (name, text) in &files {
        assert!(parse(text).is_ok(), "{name}: {:?}", parse(text).err());
    }
    let started = std::time::Instant::now();
    let checked = check_files(&files);
    let elapsed = started.elapsed();
    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.is_error()).map(ToString::to_string).collect();
    println!("{} in {elapsed:?}; {} diagnostics", checked.summary(), checked.diagnostics.len());
    for d in checked.diagnostics.iter().take(40) {
        println!("  {d}");
    }
    // The `Malformed*` operations live in the upstream `malformedRequests/`
    // folder, which is not part of the 31 files.
    assert!(errors.iter().all(|e| e.contains("unknown operation `Malformed")), "{errors:#?}");
    let m = checked.model.unwrap();
    assert_eq!(m.services().map(Shape::name).collect::<Vec<_>>(), ["RestJson"]);
    let (c, _) = m.to_collection();
    println!("{}: {} requests", c.name, c.request_count());
    // Opening any of the files in the Spec Hub brings in the RestJson model;
    // `http-accept.smithy` is a model of its own (namespace
    // `aws.protocoltests.misc`, not bound to the service).
    let sources: Vec<SourceFile> = files.iter().map(|(n, t)| SourceFile::parse(n.as_str(), t)).collect();
    let refs: Vec<&SourceFile> = sources.iter().collect();
    let accept = files.iter().position(|(n, _)| n == "http-accept.smithy").unwrap();
    for (start, (name, _)) in files.iter().enumerate() {
        let related = related_files(&refs, start);
        if start == accept {
            assert_eq!(related, [accept]);
        } else {
            assert_eq!(related.len(), 30, "{name}");
            assert!(!related.contains(&accept));
        }
    }
    assert_eq!(c.name, "Sample Rest Json Protocol Service");
    assert_eq!(c.request_count(), 80);
}

/// AWS SQS, Lambda and S3 JSON ASTs (not part of the repository): each still
/// imports on its own, and three unrelated models in one folder are not
/// merged by [`related_files`]. Run with
/// `MERPATI_SMITHY_AWS_MODELS=<dir with sqs.json lambda.json s3.json> cargo test -p merpati-convert -- --ignored`.
#[test]
#[ignore]
fn aws_models_stay_separate() {
    let dir = std::path::PathBuf::from(std::env::var("MERPATI_SMITHY_AWS_MODELS").expect("MERPATI_SMITHY_AWS_MODELS"));
    let names = ["sqs.json", "lambda.json", "s3.json"];
    let texts: Vec<String> = names.iter().map(|n| std::fs::read_to_string(dir.join(n)).unwrap()).collect();
    let started = std::time::Instant::now();
    let sources: Vec<SourceFile> = names.iter().zip(&texts).map(|(n, t)| SourceFile::parse(*n, t)).collect();
    println!("parsed 3 files in {:?}", started.elapsed());
    let refs: Vec<&SourceFile> = sources.iter().collect();
    for (i, name) in names.iter().enumerate() {
        assert_eq!(related_files(&refs, i), [i], "{name}");
        let started = std::time::Instant::now();
        let checked = check_sources(&[refs[i]]);
        println!("{name}: {} in {:?}", checked.summary(), started.elapsed());
        assert!(checked.model.is_some());
        let (c, _) = import(&texts[i]).unwrap();
        assert!(c.request_count() > 10, "{name}: {}", c.request_count());
    }
}

#[test]
fn deeply_nested_node_values_are_a_syntax_error_not_a_stack_overflow() {
    let depth = 50_000;
    for (open, close) in [("[", "]"), ("{a: ", "}")] {
        let text = format!(
            "$version: \"2\"\nnamespace a.b\n@documentation({}\"x\"{})\nstring S\n",
            open.repeat(depth),
            close.repeat(depth)
        );
        let d = syntax_error(&text);
        assert!(d.message.contains("nested"), "{}", d.message);
        assert!(check(&text).diagnostics.iter().any(Diagnostic::is_error));
    }
    // Ordinary nesting still parses.
    let ok = "$version: \"2\"\nmetadata m = {a: [{b: [1, 2]}]}\nnamespace a.b\n@tags([\"a\"])\nstring S\n";
    if let Err(d) = parse_idl(ok) {
        panic!("{}", d.message);
    }
}

#[test]
fn recursive_structures_are_cut_instead_of_growing_exponentially() {
    let text = r#"$version: "2"
namespace a.b
use aws.protocols#restJson1
@restJson1
service S { version: "1", operations: [Op] }
@http(method: "POST", uri: "/x")
operation Op { input := { filter: Filter } }
structure Filter {
    name: String
    and: FilterList
    or: FilterList
    not: Filter
    other: Filter
    @default("x")
    label: String
}
list FilterList { member: Filter }
"#;
    let (c, _) = import(text).unwrap();
    let r = http(&c, "Op");
    assert!(r.body.raw.len() < 1000, "{} bytes", r.body.raw.len());
    let body = body_json(r);
    assert_eq!(body, json!({"filter": {"name": "string", "and": [], "or": [], "label": "x"}}));
}

/// A zip archive (as jars are) of `(name, content, deflated)` entries.
fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    use std::io::Write;
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, content, deflate) in entries {
        let data = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(content).unwrap();
            e.finish().unwrap()
        } else {
            content.to_vec()
        };
        let mut crc = flate2::Crc::new();
        crc.update(content);
        let method: u16 = if *deflate { 8 } else { 0 };
        let mut sizes = Vec::new();
        sizes.extend(crc.sum().to_le_bytes());
        sizes.extend((data.len() as u32).to_le_bytes());
        sizes.extend((content.len() as u32).to_le_bytes());
        sizes.extend((name.len() as u16).to_le_bytes());
        let offset = out.len() as u32;
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend([20, 0, 0, 0]);
        out.extend(method.to_le_bytes());
        out.extend([0; 4]);
        out.extend(&sizes);
        out.extend([0; 2]);
        out.extend(name.as_bytes());
        out.extend(&data);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend([20, 0, 20, 0, 0, 0]);
        central.extend(method.to_le_bytes());
        central.extend([0; 4]);
        central.extend(&sizes);
        central.extend([0; 12]);
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let (cd_offset, cd_size) = (out.len() as u32, central.len() as u32);
    out.extend(central);
    let comment = b"test jar";
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend((comment.len() as u16).to_le_bytes());
    out.extend(comment);
    out
}

#[test]
fn jar_model_files_follow_the_manifest() {
    let jar = zip(&[
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n", false),
        ("META-INF/smithy/manifest", b"# models\nshapes.smithy\n\n  nested/more.json\nREADME.txt\n", true),
        ("META-INF/smithy/shapes.smithy", b"namespace com.example.shared\nstring Id\n", true),
        ("META-INF/smithy/nested/more.json", br#"{"smithy": "2.0", "shapes": {}}"#, false),
        ("META-INF/smithy/README.txt", b"not a model", false),
        ("META-INF/smithy/unlisted.smithy", b"namespace x\nstring Y\n", false),
    ]);
    let files = jar_model_files(&jar).unwrap();
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["shapes.smithy", "nested/more.json"]);
    assert_eq!(files[0].1, "namespace com.example.shared\nstring Id\n");

    // A plain Java jar has no models.
    assert!(jar_model_files(&zip(&[("a/B.class", b"\xca\xfe", true)])).unwrap().is_empty());
    assert!(jar_model_files(b"not a zip at all, just some text").unwrap_err().contains("not a zip"));
    let listed_missing = zip(&[("META-INF/smithy/manifest", b"gone.smithy\n", false)]);
    assert!(jar_model_files(&listed_missing).unwrap_err().contains("gone.smithy"));
    let escaping = zip(&[("META-INF/smithy/manifest", b"../evil.smithy\n", false)]);
    assert!(jar_model_files(&escaping).unwrap_err().contains("invalid model path"));
    let mut corrupt =
        zip(&[("META-INF/smithy/manifest", b"a.smithy\n", false), ("META-INF/smithy/a.smithy", b"x", false)]);
    let at = corrupt.windows(1).rposition(|w| w == b"x").unwrap();
    corrupt[at] = b'y';
    assert!(jar_model_files(&corrupt).unwrap_err().contains("CRC"));
}

#[test]
fn maven_dependencies_come_from_the_local_repository() {
    let repo = std::env::temp_dir().join(format!("merpati-smithy-m2-{}", std::process::id()));
    let put = |rel: &str, bytes: &[u8]| {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    put(
        "com/example/shapes/1.0.0/shapes-1.0.0.jar",
        &zip(&[
            ("META-INF/smithy/manifest", b"shapes.smithy\nmore.json\n", false),
            ("META-INF/smithy/shapes.smithy", b"$version: \"2\"\nnamespace com.example.shared\nstring Id\n", true),
            (
                "META-INF/smithy/more.json",
                br#"{"smithy": "2.0", "shapes": {"com.example.shared#Count": {"type": "integer"}}}"#,
                true,
            ),
        ]),
    );
    put(
        "com/example/shapes/1.0.0/shapes-1.0.0.pom",
        br#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <groupId>com.example</groupId><artifactId>shapes</artifactId><version>1.0.0</version>
  <properties><common.version>2.1</common.version></properties>
  <dependencies>
    <dependency><groupId>com.example</groupId><artifactId>common</artifactId><version>${common.version}</version></dependency>
    <dependency><groupId>${project.groupId}</groupId><artifactId>helpers</artifactId><version>${project.version}</version><scope>runtime</scope></dependency>
    <dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>4.13</version><scope>test</scope></dependency>
    <dependency><groupId>com.example</groupId><artifactId>extra</artifactId><version>1</version><optional>true</optional></dependency>
    <dependency><groupId>com.example</groupId><artifactId>shapes</artifactId><version>0.9</version></dependency>
  </dependencies>
</project>"#,
    );
    put(
        "com/example/common/2.1/common-2.1.jar",
        &zip(&[
            ("META-INF/smithy/manifest", b"common.smithy\n", false),
            ("META-INF/smithy/common.smithy", b"$version: \"2\"\nnamespace com.example.common\nstring Region\n", false),
        ]),
    );
    let build = r#"{"version": "1.0", "maven": {"dependencies": [
        "com.example:shapes:1.0.0", "com.example:missing:3.0", "bad-coordinate", "com.example:ranged:[1.0,2.0)"]}}"#;
    let deps = resolve_maven_dependencies(build, Some(&repo)).unwrap();
    let _ = std::fs::remove_dir_all(&repo);

    let names: Vec<&str> = deps.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "shapes-1.0.0.jar!/META-INF/smithy/shapes.smithy",
            "shapes-1.0.0.jar!/META-INF/smithy/more.json",
            "common-2.1.jar!/META-INF/smithy/common.smithy"
        ]
    );
    let w = &deps.warnings;
    assert_eq!(w.len(), 4, "{w:#?}");
    assert!(w[0].contains("invalid Maven dependency `bad-coordinate`"), "{w:#?}");
    assert!(w[1].starts_with("Maven dependency com.example:missing:3.0 is not in the local repository ("), "{w:#?}");
    assert!(w[1].contains(&format!(
        "missing{}3.0{}missing-3.0.jar",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    )));
    assert!(w[2].contains("com.example:ranged:[1.0,2.0)") && w[2].contains("version ranges"), "{w:#?}");
    assert!(w[3].starts_with("Maven dependency com.example:helpers:1.0.0 (needed by com.example:shapes:1.0.0)"));

    // The project's own model uses shapes of both jars.
    let project = SourceFile::parse(
        "main.smithy",
        "$version: \"2\"\nnamespace ex.app\nuse com.example.shared#Id\nuse com.example.common#Region\n\
         structure Thing { id: Id, region: Region, n: com.example.shared#Count }\n",
    );
    let errors_of = |files: &[&SourceFile]| -> Vec<String> {
        check_sources(files).diagnostics.iter().filter(|d| d.is_error()).map(ToString::to_string).collect()
    };
    assert_eq!(errors_of(&[&project]).len(), 3);
    let sources = deps.sources();
    let mut all = vec![&project];
    all.extend(sources.iter());
    assert_eq!(errors_of(&all), Vec::<String>::new());
    assert_eq!(check_sources(&all).model.unwrap().namespace, "ex.app");

    // No Maven section: nothing to do.
    assert!(resolve_maven_dependencies(r#"{"version": "1.0"}"#, Some(&repo)).unwrap().files.is_empty());
    assert!(build_config_maven_dependencies(r#"{"maven": {"dependencies": "x"}}"#).unwrap_err().contains("array"));
    assert!(build_config_maven_dependencies(r#"{"maven": {"dependencies": [1]}}"#).unwrap_err().contains("strings"));
}
