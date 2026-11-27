//! Recognize bounded Subsonic cover-error envelopes. Unknown/malformed bytes
//! remain an image failure; only an explicit missing-data response is absence.
use xml::reader::{ParserConfig2,XmlEvent};

pub(crate) fn error_code(bytes:&[u8])->Option<u32> {
    if bytes.len()>4*1024*1024 {return None;}
    if let Ok(value)=serde_json::from_slice::<serde_json::Value>(bytes) {
        let root=&value["subsonic-response"];
        return (root["status"].as_str()==Some("failed")).then(||root["error"]["code"].as_u64()).flatten().and_then(|code|u32::try_from(code).ok());
    }
    let input=std::str::from_utf8(bytes).ok()?;
    // No DTD or custom/external entity processing for an error envelope.
    if input.contains("<!DOCTYPE") {return None;}
    let reader=ParserConfig2::new().allow_multiple_root_elements(false)
        .trim_whitespace(true).ignore_comments(true).max_name_length(128)
        .max_attributes(32).max_attribute_length(4096).max_data_length(4096)
        .max_entity_expansion_length(4096).max_entity_expansion_depth(1)
        .create_reader(std::io::Cursor::new(bytes));
    let mut depth=0;let mut root=false;let mut error=false;let mut code=None;let mut namespace=None;let mut ended=false;
    for event in reader {
        match event.ok()? {
            XmlEvent::StartDocument {..}=>{},
            XmlEvent::StartElement {name,attributes,..}=>{
                if depth==0 && !root && name.local_name=="subsonic-response" && matches!(name.namespace.as_deref(),None|Some("http://subsonic.org/restapi")) {
                    if !attributes.iter().any(|attribute|attribute.name.namespace.is_none() && attribute.name.local_name=="status" && attribute.value=="failed") {return None;}
                    namespace=name.namespace;root=true;
                } else if depth==1 && root && !error && name.local_name=="error" && name.namespace==namespace {
                    code=attributes.iter().find(|attribute|attribute.name.namespace.is_none() && attribute.name.local_name=="code")?.value.trim_ascii().parse().ok();error=true;
                } else {return None;}
                depth+=1;
            }
            XmlEvent::EndElement {..}=>{depth-=1;},
            XmlEvent::Whitespace(_)=>{},
            XmlEvent::Characters(text) if text.trim_ascii().is_empty()=>{},
            XmlEvent::EndDocument=>{ended=true;},
            _=>return None,
        }
    }
    (ended && root && error && depth==0).then_some(code).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_json_xml_errors_preserve_missing_auth_and_other_failures() {
        for code in [0,10,20,30,40,41,42,43,44,50,60,70,500] {
            for quote in ['\'','"'] {
                for gap in [" ","\n\t"] {
                    let body=format!("<?xml version='1.0'?>\n<subsonic-response xmlns='http://subsonic.org/restapi' status={quote}failed{quote}><error{gap}code = {quote}{code}{quote} message='generated &amp; harmless'/></subsonic-response>");
                    assert_eq!(error_code(body.as_bytes()),Some(code));
                }
            }
            assert_eq!(error_code(format!(r#"{{"subsonic-response":{{"status":"failed","error":{{"code":{code}}}}}}}"#).as_bytes()),Some(code));
        }
        assert_eq!(error_code("\u{feff}<?xml version='1.0'?><!-- generated --><subsonic-response status='failed'><error code='70'/></subsonic-response>".as_bytes()),Some(70));
        for body in [
            "<subsonic-response status='ok'><error code='70'/></subsonic-response>",
            "<subsonic-response status='failed'><error code='70' code='40'/></subsonic-response>",
            "<error code='70'/>",
            "<?xml garbage?><subsonic-response status='failed'><error code='70'/></subsonic-response>",
            "<subsonic-response status='failed'><error code='70' message='bad & entity'/></subsonic-response>","<subsonic-response status='failed'><error code='70'/>",
            "<!DOCTYPE x SYSTEM 'file:///never-read'><subsonic-response status='failed'><error code='70'/></subsonic-response>",
            "<subsonic-response status='failed'><error code='70'/><extra/></subsonic-response>",
            r#"{"subsonic-response":{"status":"ok","error":{"code":70}}}"#,
        ] {assert_eq!(error_code(body.as_bytes()),None,"{body}");}
    }
}
