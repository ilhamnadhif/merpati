//! Dynamic variables (`{{$guid}}`, `{{$randomEmail}}`, ...), compatible with
//! the names Postman documents. Each occurrence produces a fresh value.

use rand::Rng;
use rand::seq::IndexedRandom;

const FIRST_NAMES: &[&str] = &[
    "Ava", "Liam", "Noah", "Emma", "Olivia", "Mia", "Lucas", "Ethan", "Sofia", "Aria", "Budi", "Siti", "Rina", "Agus",
    "Dewi", "Hana", "Kenji", "Yuki", "Mateo", "Lena", "Omar", "Zara", "Ivan", "Nina", "Leo", "Maya",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Brown", "Garcia", "Miller", "Davis", "Wilson", "Anderson", "Thomas", "Moore", "Santoso",
    "Wijaya", "Pratama", "Saputra", "Nguyen", "Kim", "Tanaka", "Rossi", "Muller", "Silva", "Novak", "Haddad",
];
const NAME_PREFIXES: &[&str] = &["Mr.", "Mrs.", "Ms.", "Miss", "Dr."];
const NAME_SUFFIXES: &[&str] = &["Jr.", "Sr.", "I", "II", "III", "IV", "V", "MD", "DDS", "PhD", "DVM"];
const JOB_AREAS: &[&str] = &[
    "Solutions",
    "Program",
    "Brand",
    "Security",
    "Research",
    "Marketing",
    "Directives",
    "Implementation",
    "Integration",
    "Functionality",
    "Response",
    "Paradigm",
    "Tactics",
    "Identity",
    "Markets",
    "Group",
];
const JOB_DESCRIPTORS: &[&str] = &[
    "Lead",
    "Senior",
    "Direct",
    "Corporate",
    "Dynamic",
    "Future",
    "Product",
    "National",
    "Regional",
    "District",
    "Central",
    "Global",
    "Customer",
    "Investor",
    "Principal",
    "Internal",
    "International",
    "Chief",
    "Legacy",
];
const JOB_TYPES: &[&str] = &[
    "Supervisor",
    "Associate",
    "Executive",
    "Liaison",
    "Officer",
    "Manager",
    "Engineer",
    "Specialist",
    "Director",
    "Coordinator",
    "Administrator",
    "Architect",
    "Analyst",
    "Designer",
    "Planner",
    "Technician",
    "Developer",
];
const CITIES: &[&str] = &[
    "Jakarta",
    "Bandung",
    "Surabaya",
    "Yogyakarta",
    "Denpasar",
    "London",
    "Paris",
    "Berlin",
    "Tokyo",
    "Osaka",
    "New York",
    "Austin",
    "Toronto",
    "Sydney",
    "Madrid",
    "Lisbon",
    "Nairobi",
    "Seoul",
    "Singapore",
    "Oslo",
];
const STREETS: &[&str] = &[
    "Main",
    "Oak",
    "Pine",
    "Maple",
    "Cedar",
    "Elm",
    "Sudirman",
    "Thamrin",
    "Gatot Subroto",
    "Diponegoro",
    "Lake",
    "Hill",
    "Sunset",
    "Park",
    "River",
    "Washington",
    "Lincoln",
];
const STREET_SUFFIXES: &[&str] = &["Street", "Avenue", "Road", "Lane", "Boulevard", "Drive", "Way", "Court"];
const COUNTRIES: &[(&str, &str)] = &[
    ("Indonesia", "ID"),
    ("United States", "US"),
    ("United Kingdom", "GB"),
    ("Germany", "DE"),
    ("France", "FR"),
    ("Japan", "JP"),
    ("Brazil", "BR"),
    ("Canada", "CA"),
    ("Australia", "AU"),
    ("India", "IN"),
    ("Kenya", "KE"),
    ("Spain", "ES"),
    ("Italy", "IT"),
    ("Netherlands", "NL"),
    ("Singapore", "SG"),
    ("South Korea", "KR"),
];
const LOCALES: &[&str] = &["en", "id", "de", "fr", "ja", "es", "pt_BR", "it", "nl", "ko", "zh_CN", "ru", "sv"];
const COLORS: &[&str] = &[
    "red",
    "green",
    "blue",
    "yellow",
    "purple",
    "orange",
    "black",
    "white",
    "gold",
    "silver",
    "teal",
    "violet",
    "indigo",
    "lime",
    "maroon",
    "navy",
    "olive",
    "orchid",
    "pink",
    "plum",
    "salmon",
    "tan",
    "turquoise",
];
const ABBREVIATIONS: &[&str] = &[
    "TCP", "HTTP", "SDD", "RAM", "GB", "CSS", "SSL", "AGP", "SQL", "FTP", "PCI", "AI", "ADP", "RSS", "XML", "EXE",
    "COM", "HDD", "THX", "SMTP", "SMS", "USB", "PNG", "SAS", "IB", "SCSI", "JSON", "XSS", "JBOD",
];
const PROTOCOLS: &[&str] = &["http", "https"];
const DOMAIN_SUFFIXES: &[&str] = &["com", "net", "org", "io", "info", "biz", "name", "co.id", "dev"];
const COMPANY_SUFFIXES: &[&str] = &["Inc", "LLC", "Group", "and Sons", "Ltd", "PT", "Tbk"];
const BS_ADJECTIVES: &[&str] = &[
    "clicks-and-mortar",
    "value-added",
    "vertical",
    "proactive",
    "robust",
    "revolutionary",
    "scalable",
    "leading-edge",
    "innovative",
    "intuitive",
    "strategic",
    "e-business",
    "mission-critical",
    "sticky",
];
const BS_BUZZ: &[&str] = &[
    "synergize",
    "strategize",
    "reinvent",
    "harness",
    "engage",
    "transition",
    "embrace",
    "enable",
    "orchestrate",
    "leverage",
    "streamline",
    "incubate",
    "scale",
    "monetize",
    "empower",
    "visualize",
    "integrate",
];
const BS_NOUNS: &[&str] = &[
    "synergies",
    "paradigms",
    "markets",
    "partnerships",
    "infrastructures",
    "platforms",
    "initiatives",
    "channels",
    "communities",
    "solutions",
    "e-tailers",
    "web services",
    "action-items",
    "portals",
    "niches",
];
const CATCH_ADJ: &[&str] = &[
    "Adaptive",
    "Advanced",
    "Ameliorated",
    "Assimilated",
    "Automated",
    "Balanced",
    "Business-focused",
    "Centralized",
    "Cloned",
    "Compatible",
    "Configurable",
    "Cross-platform",
    "Customer-focused",
    "Decentralized",
];
const CATCH_DESC: &[&str] = &[
    "24 hour",
    "24/7",
    "3rd generation",
    "4th generation",
    "5th generation",
    "6th generation",
    "actuating",
    "analyzing",
    "asymmetric",
    "asynchronous",
    "attitude-oriented",
    "background",
    "bandwidth-monitored",
];
const CATCH_NOUNS: &[&str] = &[
    "ability",
    "access",
    "adapter",
    "algorithm",
    "alliance",
    "analyzer",
    "application",
    "approach",
    "architecture",
    "archive",
    "artificial intelligence",
    "array",
    "attitude",
    "benchmark",
    "capability",
];
const DB_COLUMNS: &[&str] = &[
    "id",
    "title",
    "name",
    "email",
    "phone",
    "token",
    "group",
    "category",
    "password",
    "comment",
    "avatar",
    "status",
    "createdAt",
    "updatedAt",
];
const DB_TYPES: &[&str] = &[
    "int",
    "varchar",
    "text",
    "date",
    "datetime",
    "tinyint",
    "time",
    "timestamp",
    "smallint",
    "mediumint",
    "bigint",
    "decimal",
    "float",
    "double",
    "real",
    "bit",
    "boolean",
    "serial",
    "blob",
    "binary",
    "enum",
];
const DB_COLLATIONS: &[&str] = &[
    "utf8_unicode_ci",
    "utf8_general_ci",
    "utf8_bin",
    "ascii_bin",
    "ascii_general_ci",
    "cp1250_bin",
    "cp1250_general_ci",
];
const DB_ENGINES: &[&str] = &["InnoDB", "MyISAM", "MEMORY", "CSV", "BLACKHOLE", "ARCHIVE"];
const WEEKDAYS: &[&str] = &["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: &[&str] = &[
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const FILE_EXTS: &[(&str, &str)] = &[
    ("json", "application/json"),
    ("txt", "text/plain"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("pdf", "application/pdf"),
    ("csv", "text/csv"),
    ("xml", "application/xml"),
    ("html", "text/html"),
    ("mp4", "video/mp4"),
    ("mp3", "audio/mpeg"),
    ("zip", "application/zip"),
    ("gif", "image/gif"),
];
const FILE_TYPES: &[&str] = &["application", "audio", "image", "text", "video", "font", "model"];
const PRODUCTS: &[&str] = &[
    "Chair", "Car", "Computer", "Keyboard", "Mouse", "Bike", "Ball", "Gloves", "Pants", "Shirt", "Table", "Shoes",
    "Hat", "Towels", "Soap", "Tuna", "Chicken", "Fish", "Cheese", "Bacon", "Pizza", "Salad", "Sausages", "Chips",
];
const PRODUCT_ADJECTIVES: &[&str] = &[
    "Small",
    "Ergonomic",
    "Rustic",
    "Intelligent",
    "Gorgeous",
    "Incredible",
    "Fantastic",
    "Practical",
    "Sleek",
    "Awesome",
    "Generic",
    "Handcrafted",
    "Handmade",
    "Licensed",
    "Refined",
    "Unbranded",
    "Tasty",
];
const PRODUCT_MATERIALS: &[&str] =
    &["Steel", "Wooden", "Concrete", "Plastic", "Cotton", "Granite", "Rubber", "Metal", "Soft", "Fresh", "Frozen"];
const DEPARTMENTS: &[&str] = &[
    "Books",
    "Movies",
    "Music",
    "Games",
    "Electronics",
    "Computers",
    "Home",
    "Garden",
    "Tools",
    "Grocery",
    "Health",
    "Beauty",
    "Toys",
    "Kids",
    "Baby",
    "Clothing",
    "Shoes",
    "Jewelery",
    "Sports",
    "Outdoors",
    "Automotive",
    "Industrial",
];
const NOUNS: &[&str] = &[
    "driver",
    "protocol",
    "bandwidth",
    "panel",
    "microchip",
    "program",
    "port",
    "card",
    "array",
    "interface",
    "system",
    "sensor",
    "firewall",
    "hard drive",
    "pixel",
    "alarm",
    "feed",
    "monitor",
    "application",
    "transmitter",
    "bus",
    "circuit",
    "capacitor",
    "matrix",
];
const VERBS: &[&str] = &[
    "back up",
    "bypass",
    "hack",
    "override",
    "compress",
    "copy",
    "navigate",
    "index",
    "connect",
    "generate",
    "quantify",
    "calculate",
    "synthesize",
    "input",
    "transmit",
    "program",
    "reboot",
    "parse",
];
const ING_VERBS: &[&str] = &[
    "backing up",
    "bypassing",
    "hacking",
    "overriding",
    "compressing",
    "copying",
    "navigating",
    "indexing",
    "connecting",
    "generating",
    "quantifying",
    "calculating",
    "synthesizing",
    "transmitting",
    "programming",
    "parsing",
];
const ADJECTIVES: &[&str] = &[
    "auxiliary",
    "primary",
    "back-end",
    "digital",
    "open-source",
    "virtual",
    "cross-platform",
    "redundant",
    "online",
    "haptic",
    "multi-byte",
    "bluetooth",
    "wireless",
    "1080p",
    "neural",
    "optical",
    "solid state",
    "mobile",
];
const LOREM: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
    "velit",
    "esse",
    "cillum",
    "fugiat",
    "nulla",
    "pariatur",
    "excepteur",
    "sint",
    "occaecat",
    "cupidatat",
    "non",
    "proident",
    "sunt",
    "culpa",
    "qui",
    "officia",
    "deserunt",
    "mollit",
    "anim",
    "id",
    "est",
    "laborum",
];
const CURRENCIES: &[(&str, &str, &str)] = &[
    ("USD", "US Dollar", "$"),
    ("EUR", "Euro", "€"),
    ("IDR", "Rupiah", "Rp"),
    ("JPY", "Yen", "¥"),
    ("GBP", "Pound Sterling", "£"),
    ("INR", "Indian Rupee", "₹"),
    ("KRW", "Won", "₩"),
    ("BRL", "Brazilian Real", "R$"),
];
const TRANSACTION_TYPES: &[&str] = &["deposit", "withdrawal", "payment", "invoice"];
const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
];
const IMAGE_TOPICS: &[&str] = &[
    "abstract",
    "animals",
    "business",
    "cats",
    "city",
    "food",
    "nightlife",
    "fashion",
    "people",
    "nature",
    "sports",
    "technics",
    "transport",
];

/// Every supported dynamic variable name (with the leading `$`).
pub const NAMES: &[&str] = &[
    "$guid",
    "$timestamp",
    "$isoTimestamp",
    "$randomUUID",
    "$randomAlphaNumeric",
    "$randomBoolean",
    "$randomInt",
    "$randomColor",
    "$randomHexColor",
    "$randomAbbreviation",
    "$randomIP",
    "$randomIPV6",
    "$randomMACAddress",
    "$randomPassword",
    "$randomLocale",
    "$randomUserAgent",
    "$randomProtocol",
    "$randomSemver",
    "$randomFirstName",
    "$randomLastName",
    "$randomFullName",
    "$randomNamePrefix",
    "$randomNameSuffix",
    "$randomJobArea",
    "$randomJobDescriptor",
    "$randomJobTitle",
    "$randomJobType",
    "$randomPhoneNumber",
    "$randomPhoneNumberExt",
    "$randomCity",
    "$randomStreetName",
    "$randomStreetAddress",
    "$randomCountry",
    "$randomCountryCode",
    "$randomLatitude",
    "$randomLongitude",
    "$randomAvatarImage",
    "$randomImageUrl",
    "$randomAbstractImage",
    "$randomAnimalsImage",
    "$randomBusinessImage",
    "$randomCatsImage",
    "$randomCityImage",
    "$randomFoodImage",
    "$randomNightlifeImage",
    "$randomFashionImage",
    "$randomPeopleImage",
    "$randomNatureImage",
    "$randomSportsImage",
    "$randomTransportImage",
    "$randomImageDataUri",
    "$randomBankAccount",
    "$randomBankAccountName",
    "$randomCreditCardMask",
    "$randomBankAccountBic",
    "$randomBankAccountIban",
    "$randomTransactionType",
    "$randomCurrencyCode",
    "$randomCurrencyName",
    "$randomCurrencySymbol",
    "$randomBitcoin",
    "$randomCompanyName",
    "$randomCompanySuffix",
    "$randomBs",
    "$randomBsAdjective",
    "$randomBsBuzz",
    "$randomBsNoun",
    "$randomCatchPhrase",
    "$randomCatchPhraseAdjective",
    "$randomCatchPhraseDescriptor",
    "$randomCatchPhraseNoun",
    "$randomDatabaseColumn",
    "$randomDatabaseType",
    "$randomDatabaseCollation",
    "$randomDatabaseEngine",
    "$randomDateFuture",
    "$randomDatePast",
    "$randomDateRecent",
    "$randomWeekday",
    "$randomMonth",
    "$randomDomainName",
    "$randomDomainSuffix",
    "$randomDomainWord",
    "$randomEmail",
    "$randomExampleEmail",
    "$randomUserName",
    "$randomUrl",
    "$randomFileName",
    "$randomFileType",
    "$randomFileExt",
    "$randomCommonFileName",
    "$randomCommonFileType",
    "$randomCommonFileExt",
    "$randomFilePath",
    "$randomDirectoryPath",
    "$randomMimeType",
    "$randomPrice",
    "$randomProduct",
    "$randomProductAdjective",
    "$randomProductMaterial",
    "$randomProductName",
    "$randomDepartment",
    "$randomNoun",
    "$randomVerb",
    "$randomIngverb",
    "$randomAdjective",
    "$randomWord",
    "$randomWords",
    "$randomPhrase",
    "$randomLoremWord",
    "$randomLoremWords",
    "$randomLoremSentence",
    "$randomLoremSentences",
    "$randomLoremParagraph",
    "$randomLoremParagraphs",
    "$randomLoremText",
    "$randomLoremSlug",
    "$randomLoremLines",
];

fn pick<'a>(list: &[&'a str]) -> &'a str {
    list.choose(&mut rand::rng()).copied().unwrap_or("")
}

fn digits(n: usize) -> String {
    let mut rng = rand::rng();
    (0..n).map(|_| char::from(b'0' + rng.random_range(0..10u8))).collect()
}

fn alnum(n: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..n).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

fn lorem_words(n: usize) -> String {
    (0..n).map(|_| pick(LOREM)).collect::<Vec<_>>().join(" ")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn lorem_sentence() -> String {
    let n = rand::rng().random_range(5..12);
    format!("{}.", capitalize(&lorem_words(n)))
}

fn lorem_paragraph() -> String {
    let n = rand::rng().random_range(3..6);
    (0..n).map(|_| lorem_sentence()).collect::<Vec<_>>().join(" ")
}

fn word() -> &'static str {
    let mut rng = rand::rng();
    match rng.random_range(0..4) {
        0 => pick(NOUNS),
        1 => pick(ADJECTIVES),
        2 => pick(VERBS),
        _ => pick(BS_NOUNS),
    }
}

fn domain_word() -> String {
    format!("{}{}", pick(LAST_NAMES).to_lowercase(), pick(&["", "-tech", "-labs", "corp", "hub"]))
}

fn date_offset(days_min: i64, days_max: i64) -> String {
    let mut rng = rand::rng();
    let secs = rng.random_range(days_min * 86_400..days_max * 86_400);
    let t = chrono::Utc::now() + chrono::Duration::seconds(secs);
    t.to_rfc2822()
}

fn hex_bytes(n: usize, sep: &str) -> String {
    let mut rng = rand::rng();
    (0..n).map(|_| format!("{:02x}", rng.random::<u8>())).collect::<Vec<_>>().join(sep)
}

fn image_url(topic: &str) -> String {
    let _ = topic;
    let mut rng = rand::rng();
    format!("https://picsum.photos/seed/{}/640/480", rng.random_range(1..10_000))
}

/// Generates a value for a dynamic variable name (including the `$`).
/// Returns `None` for unknown names so they stay unresolved.
pub fn generate(name: &str) -> Option<String> {
    let mut rng = rand::rng();
    let v = match name {
        "$guid" | "$randomUUID" => uuid::Uuid::new_v4().to_string(),
        "$timestamp" => chrono::Utc::now().timestamp().to_string(),
        "$isoTimestamp" => chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "$randomAlphaNumeric" => alnum(1),
        "$randomBoolean" => rng.random_bool(0.5).to_string(),
        "$randomInt" => rng.random_range(0..=1000).to_string(),
        "$randomColor" => pick(COLORS).into(),
        "$randomHexColor" => format!("#{}", hex_bytes(3, "")),
        "$randomAbbreviation" => pick(ABBREVIATIONS).into(),
        "$randomIP" => format!(
            "{}.{}.{}.{}",
            rng.random_range(1..=254u8),
            rng.random::<u8>(),
            rng.random::<u8>(),
            rng.random_range(1..=254u8)
        ),
        "$randomIPV6" => (0..8).map(|_| format!("{:x}", rng.random::<u16>())).collect::<Vec<_>>().join(":"),
        "$randomMACAddress" => hex_bytes(6, ":"),
        "$randomPassword" => alnum(15),
        "$randomLocale" => pick(LOCALES).into(),
        "$randomUserAgent" => pick(USER_AGENTS).into(),
        "$randomProtocol" => pick(PROTOCOLS).into(),
        "$randomSemver" => format!("{}.{}.{}", rng.random_range(0..10), rng.random_range(0..20), rng.random_range(0..50)),
        "$randomFirstName" => pick(FIRST_NAMES).into(),
        "$randomLastName" => pick(LAST_NAMES).into(),
        "$randomFullName" => format!("{} {}", pick(FIRST_NAMES), pick(LAST_NAMES)),
        "$randomNamePrefix" => pick(NAME_PREFIXES).into(),
        "$randomNameSuffix" => pick(NAME_SUFFIXES).into(),
        "$randomJobArea" => pick(JOB_AREAS).into(),
        "$randomJobDescriptor" => pick(JOB_DESCRIPTORS).into(),
        "$randomJobTitle" => format!("{} {} {}", pick(JOB_DESCRIPTORS), pick(JOB_AREAS), pick(JOB_TYPES)),
        "$randomJobType" => pick(JOB_TYPES).into(),
        "$randomPhoneNumber" => format!("{}-{}-{}", digits(3), digits(3), digits(4)),
        "$randomPhoneNumberExt" => format!("{}-{}-{}-{}", digits(2), digits(3), digits(3), digits(4)),
        "$randomCity" => pick(CITIES).into(),
        "$randomStreetName" => format!("{} {}", pick(STREETS), pick(STREET_SUFFIXES)),
        "$randomStreetAddress" => format!("{} {} {}", rng.random_range(1..9999), pick(STREETS), pick(STREET_SUFFIXES)),
        "$randomCountry" => COUNTRIES.choose(&mut rng).map(|c| c.0).unwrap_or_default().into(),
        "$randomCountryCode" => COUNTRIES.choose(&mut rng).map(|c| c.1).unwrap_or_default().into(),
        "$randomLatitude" => format!("{:.4}", rng.random_range(-90.0..90.0f64)),
        "$randomLongitude" => format!("{:.4}", rng.random_range(-180.0..180.0f64)),
        "$randomAvatarImage" => format!("https://i.pravatar.cc/300?img={}", rng.random_range(1..70)),
        "$randomImageUrl" => image_url("any"),
        "$randomAbstractImage" | "$randomAnimalsImage" | "$randomBusinessImage" | "$randomCatsImage"
        | "$randomCityImage" | "$randomFoodImage" | "$randomNightlifeImage" | "$randomFashionImage"
        | "$randomPeopleImage" | "$randomNatureImage" | "$randomSportsImage" | "$randomTransportImage" => {
            let topic = name.trim_start_matches("$random").trim_end_matches("Image").to_lowercase();
            image_url(IMAGE_TOPICS.iter().find(|t| **t == topic).copied().unwrap_or("abstract"))
        }
        "$randomImageDataUri" => {
            "data:image/svg+xml;charset=UTF-8,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%2240%22%20height%3D%2240%22%3E%3Crect%20width%3D%2240%22%20height%3D%2240%22%20fill%3D%22grey%22%2F%3E%3C%2Fsvg%3E".into()
        }
        "$randomBankAccount" => digits(8),
        "$randomBankAccountName" => format!("{} Account", pick(&["Checking", "Savings", "Money Market", "Investment", "Home Loan", "Credit Card"])),
        "$randomCreditCardMask" => digits(4),
        "$randomBankAccountBic" => format!("{}{}{}", alnum(4).to_uppercase(), pick(&["US", "ID", "DE", "GB"]), alnum(2).to_uppercase()),
        "$randomBankAccountIban" => format!("{}{}{}", pick(&["DE", "FR", "NL", "GB"]), digits(2), digits(18)),
        "$randomTransactionType" => pick(TRANSACTION_TYPES).into(),
        "$randomCurrencyCode" => CURRENCIES.choose(&mut rng).map(|c| c.0).unwrap_or_default().into(),
        "$randomCurrencyName" => CURRENCIES.choose(&mut rng).map(|c| c.1).unwrap_or_default().into(),
        "$randomCurrencySymbol" => CURRENCIES.choose(&mut rng).map(|c| c.2).unwrap_or_default().into(),
        "$randomBitcoin" => format!("1{}", alnum(33)),
        "$randomCompanyName" => format!("{} {}", pick(LAST_NAMES), pick(COMPANY_SUFFIXES)),
        "$randomCompanySuffix" => pick(COMPANY_SUFFIXES).into(),
        "$randomBs" => format!("{} {} {}", pick(BS_BUZZ), pick(BS_ADJECTIVES), pick(BS_NOUNS)),
        "$randomBsAdjective" => pick(BS_ADJECTIVES).into(),
        "$randomBsBuzz" => pick(BS_BUZZ).into(),
        "$randomBsNoun" => pick(BS_NOUNS).into(),
        "$randomCatchPhrase" => format!("{} {} {}", pick(CATCH_ADJ), pick(CATCH_DESC), pick(CATCH_NOUNS)),
        "$randomCatchPhraseAdjective" => pick(CATCH_ADJ).into(),
        "$randomCatchPhraseDescriptor" => pick(CATCH_DESC).into(),
        "$randomCatchPhraseNoun" => pick(CATCH_NOUNS).into(),
        "$randomDatabaseColumn" => pick(DB_COLUMNS).into(),
        "$randomDatabaseType" => pick(DB_TYPES).into(),
        "$randomDatabaseCollation" => pick(DB_COLLATIONS).into(),
        "$randomDatabaseEngine" => pick(DB_ENGINES).into(),
        "$randomDateFuture" => date_offset(1, 365),
        "$randomDatePast" => date_offset(-365, -1),
        "$randomDateRecent" => date_offset(-2, 0),
        "$randomWeekday" => pick(WEEKDAYS).into(),
        "$randomMonth" => pick(MONTHS).into(),
        "$randomDomainName" => format!("{}.{}", domain_word(), pick(DOMAIN_SUFFIXES)),
        "$randomDomainSuffix" => pick(DOMAIN_SUFFIXES).into(),
        "$randomDomainWord" => domain_word(),
        "$randomEmail" => format!(
            "{}.{}@{}.{}",
            pick(FIRST_NAMES).to_lowercase(),
            pick(LAST_NAMES).to_lowercase(),
            pick(&["gmail", "yahoo", "hotmail", "outlook", "proton"]),
            "com"
        ),
        "$randomExampleEmail" => format!(
            "{}{}@example.{}",
            pick(FIRST_NAMES).to_lowercase(),
            rng.random_range(1..100),
            pick(&["com", "net", "org"])
        ),
        "$randomUserName" => format!("{}.{}{}", pick(FIRST_NAMES), pick(LAST_NAMES), rng.random_range(1..100)),
        "$randomUrl" => format!("{}://{}.{}", pick(PROTOCOLS), domain_word(), pick(DOMAIN_SUFFIXES)),
        "$randomFileName" | "$randomCommonFileName" => {
            let (ext, _) = FILE_EXTS.choose(&mut rng).copied().unwrap_or(("txt", "text/plain"));
            format!("{}_{}.{}", pick(NOUNS).replace(' ', "_"), pick(LOREM), ext)
        }
        "$randomFileType" | "$randomCommonFileType" => pick(FILE_TYPES).into(),
        "$randomFileExt" | "$randomCommonFileExt" => FILE_EXTS.choose(&mut rng).map(|e| e.0).unwrap_or("txt").into(),
        "$randomFilePath" => format!("/{}/{}/{}.{}", pick(LOREM), pick(LOREM), pick(NOUNS).replace(' ', "_"), "txt"),
        "$randomDirectoryPath" => format!("/{}/{}", pick(&["usr", "var", "home", "opt", "etc"]), pick(LOREM)),
        "$randomMimeType" => FILE_EXTS.choose(&mut rng).map(|e| e.1).unwrap_or("text/plain").into(),
        "$randomPrice" => format!("{}.{:02}", rng.random_range(1..1000), rng.random_range(0..100)),
        "$randomProduct" => pick(PRODUCTS).into(),
        "$randomProductAdjective" => pick(PRODUCT_ADJECTIVES).into(),
        "$randomProductMaterial" => pick(PRODUCT_MATERIALS).into(),
        "$randomProductName" => format!("{} {} {}", pick(PRODUCT_ADJECTIVES), pick(PRODUCT_MATERIALS), pick(PRODUCTS)),
        "$randomDepartment" => pick(DEPARTMENTS).into(),
        "$randomNoun" => pick(NOUNS).into(),
        "$randomVerb" => pick(VERBS).into(),
        "$randomIngverb" => pick(ING_VERBS).into(),
        "$randomAdjective" => pick(ADJECTIVES).into(),
        "$randomWord" => word().into(),
        "$randomWords" => (0..rng.random_range(2..5)).map(|_| word()).collect::<Vec<_>>().join(" "),
        "$randomPhrase" => format!(
            "If we {} the {}, we can get to the {} {} through the {} {}!",
            pick(VERBS),
            pick(NOUNS),
            pick(ADJECTIVES),
            pick(NOUNS),
            pick(ADJECTIVES),
            pick(NOUNS)
        ),
        "$randomLoremWord" => pick(LOREM).into(),
        "$randomLoremWords" => lorem_words(3),
        "$randomLoremSentence" => lorem_sentence(),
        "$randomLoremSentences" => (0..rng.random_range(2..6)).map(|_| lorem_sentence()).collect::<Vec<_>>().join(" "),
        "$randomLoremParagraph" => lorem_paragraph(),
        "$randomLoremParagraphs" => (0..3).map(|_| lorem_paragraph()).collect::<Vec<_>>().join("\n \r"),
        "$randomLoremText" => lorem_paragraph(),
        "$randomLoremSlug" => lorem_words(3).replace(' ', "-"),
        "$randomLoremLines" => (0..rng.random_range(1..5)).map(|_| lorem_sentence()).collect::<Vec<_>>().join("\n"),
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_generates() {
        for name in NAMES {
            assert!(generate(name).is_some(), "{name} not implemented");
        }
    }

    #[test]
    fn formats() {
        assert!(generate("$randomEmail").unwrap().contains('@'));
        assert_eq!(generate("$randomMACAddress").unwrap().len(), 17);
        let n: i64 = generate("$randomInt").unwrap().parse().unwrap();
        assert!((0..=1000).contains(&n));
        assert!(generate("$isoTimestamp").unwrap().ends_with('Z'));
    }
}
