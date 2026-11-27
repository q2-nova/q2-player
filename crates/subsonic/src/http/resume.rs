//! Exact admission for resumed direct media. Never append a full response or
//! one belonging to a different representation.
use super::Response;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Validator { pub etag: String, pub total: u64 }
#[derive(Debug, PartialEq, Eq)]
pub enum Admission { Restart(Option<Validator>), Append, RetryFromZero }

/// Strong ETags are preferred; Last-Modified is admitted separately only when
/// the same response Date makes it a strong validator.
pub fn strong_etag(value: &str) -> Option<String> {
    let b=value.as_bytes();
    if !(2..=1024).contains(&b.len()) || b[0]!=b'"' || *b.last()?!=b'"' || b[1..b.len()-1].iter().any(|v| *v<0x21 || *v==b'"' || *v==0x7f) {return None;}
    Some(value.to_owned())
}
fn single<'a>(response:&'a Response,name:&str)->Option<&'a str> {
    let values=response.headers.get_all(name);let mut iter=values.iter();
    let value=iter.next()?;if iter.next().is_some() {return None;}
    std::str::from_utf8(value.as_bytes()).ok()
}
fn identity(response:&Response)->bool {
    response.headers.get_all("Content-Encoding").iter().next().is_none() || single(response,"Content-Encoding").is_some_and(|v|v.eq_ignore_ascii_case("identity"))
}
pub fn admit(response:&Response, offset:u64, previous:Option<&Validator>) -> Result<Admission,()> {
    if !identity(response) {return Err(());}
    match response.status() {
        200 => {
            let value=single(response,"ETag").and_then(strong_etag).or_else(|| {
                let modified=single(response,"Last-Modified")?;
                let date=single(response,"Date")?;
                super::date_validator::strong_last_modified(modified,date).then(||modified.to_owned())
            });
            let validator=value
                .zip(single(response,"Content-Length").and_then(|v|v.parse::<u64>().ok()))
                .filter(|(_,size)|*size>0).map(|(etag,total)|Validator {etag,total});
            Ok(Admission::Restart(validator))
        }
        206 => {
            let previous=previous.ok_or(())?;
            if offset==0 || offset>=previous.total {return Err(());}
            let matches=if strong_etag(&previous.etag).is_some() {
                single(response,"ETag").and_then(strong_etag).as_deref()==Some(previous.etag.as_str())
            } else {single(response,"Last-Modified")==Some(previous.etag.as_str())};
            if !matches {return Err(());}
            let range=format!("bytes {offset}-{}/{}",previous.total-1,previous.total);
            if single(response,"Content-Range")!=Some(range.as_str()) || single(response,"Content-Length").and_then(|v|v.parse::<u64>().ok())!=Some(previous.total-offset) {return Err(());}
            Ok(Admission::Append)
        }
        412 | 416 if offset>0 => Ok(Admission::RetryFromZero),
        _=>Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(headers: &'static str, status: &'static str) -> super::super::Response {
        use std::io::{BufRead,BufReader,Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url=format!("http://{}/generated",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            let (mut stream,_)=listener.accept().unwrap();let mut reader=BufReader::new(stream.try_clone().unwrap());
            loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}}
            write!(stream,"HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").unwrap();
        });
        let result=super::super::block_on(async move {
            match super::super::AgentBuilder::new().build().get(&url).send(Default::default()).await {
                Ok(response)=>response,
                Err(super::super::Error::Status(_,response))=>response,
                Err(error)=>panic!("generated server failed: {error}"),
            }
        }).unwrap();
        server.join().unwrap();result
    }
    #[test]
    fn range_admission_requires_matching_strong_tag_exact_range_and_identity() {
        let prior=Validator {etag:"\"v1\"".into(),total:100};
        assert_eq!(admit(&response("Content-Length: 60\r\nContent-Range: bytes 40-99/100\r\nETag: \"v1\"\r\n","206 Partial Content"),40,Some(&prior)),Ok(Admission::Append));
        for headers in [
            "Content-Length: 60\r\nContent-Range: bytes 40-99/100\r\nETag: \"v2\"\r\n",
            "Content-Length: 60\r\nContent-Range: bytes 41-99/100\r\nETag: \"v1\"\r\n",
            "Content-Length: 60\r\nContent-Range: bytes 40-99/100\r\nETag: W/\"v1\"\r\n",
            "Content-Length: 60\r\nContent-Range: bytes 40-99/100\r\nETag: \"v1\"\r\nContent-Encoding: gzip\r\n",
        ] {assert_eq!(admit(&response(headers,"206 Partial Content"),40,Some(&prior)),Err(()));}
        assert_eq!(admit(&response("Content-Length: 100\r\nETag: \"v2\"\r\n","200 OK"),40,Some(&prior)),Ok(Admission::Restart(Some(Validator {etag:"\"v2\"".into(),total:100}))));
        assert_eq!(admit(&response("Content-Length: 100\r\n","200 OK"),40,Some(&prior)),Ok(Admission::Restart(None)));
        assert_eq!(admit(&response("Content-Length: 0\r\n","416 Range Not Satisfiable"),40,Some(&prior)),Ok(Admission::RetryFromZero));
    }
    #[test]
    fn last_modified_requires_qualified_date_and_matching_partial() {
        let modified="Sun, 06 Nov 1994 08:49:37 GMT";
        let initial=response("Content-Length: 100\r\nETag: W/\"weak\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:51:37 GMT\r\n","200 OK");
        let prior=Validator {etag:modified.into(),total:100};
        assert_eq!(admit(&initial,0,None),Ok(Admission::Restart(Some(prior.clone()))));
        let partial=response("Content-Length: 60\r\nContent-Range: bytes 40-99/100\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n","206 Partial Content");
        assert_eq!(admit(&partial,40,Some(&prior)),Ok(Admission::Append));
        assert_eq!(admit(&response("Content-Length: 0\r\n","412 Precondition Failed"),40,Some(&prior)),Ok(Admission::RetryFromZero));
        let weak=response("Content-Length: 100\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:49:38 GMT\r\n","200 OK");
        assert_eq!(admit(&weak,0,None),Ok(Admission::Restart(None)));
    }
    #[test]
    fn strong_tags_refuse_weak_malformed_and_oversize() {
        assert_eq!(strong_etag("\"generated-v1\"").as_deref(),Some("\"generated-v1\""));
        for tag in ["W/\"weak\"", "unquoted", "\"a b\"", "\"two\"tags\""] {assert!(strong_etag(tag).is_none());}
        assert!(strong_etag(&format!("\"{}\"","a".repeat(1024))).is_none());
    }
}
