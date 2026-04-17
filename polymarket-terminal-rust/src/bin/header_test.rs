use reqwest::header::HeaderName;

fn main() {
    // Check what reqwest does to header names
    let name = HeaderName::from_bytes(b"POLY_ADDRESS").unwrap();
    println!("HeaderName from 'POLY_ADDRESS': {}", name.as_str());
    
    let name2 = HeaderName::from_bytes(b"POLY_SIGNATURE").unwrap();
    println!("HeaderName from 'POLY_SIGNATURE': {}", name2.as_str());
}
