//! Public embedding and document contracts. Admission, backend failures, and
//! cache-policy tests live with `Embedder`; session tests count shared initialization.

#[cfg(test)]
mod tests {
    use crate::extras::js::skills::embed::{
        DeterministicBackend, Embedder, EmbeddingBackend, EmbeddingError, SkillDocument,
    };

    #[test]
    fn deterministic_vectors_are_finite_normalized_and_independent_of_batch_position() {
        let backend = DeterministicBackend::new();
        let embedder = Embedder::new().unwrap();
        let documents = ["hello world", "goodbye world", "λ雪"].map(str::to_string);
        let vectors = backend.embed_documents(&documents).unwrap();
        assert_eq!(vectors.len(), documents.len());
        assert_eq!(backend.embed_documents(&documents).unwrap(), vectors);
        assert_eq!(embedder.embed_documents(&documents).unwrap(), vectors);
        for (document, vector) in documents.iter().zip(&vectors) {
            assert_eq!(vector.len(), 384);
            assert!(vector.iter().all(|value| value.is_finite()));
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "norm={norm}");
            assert_eq!(backend.embed_query(document).unwrap(), *vector);
            assert_eq!(
                backend
                    .embed_documents(std::slice::from_ref(document))
                    .unwrap(),
                vec![vector.clone()]
            );
        }
        assert_ne!(vectors[0], vectors[1]);
        assert_ne!(vectors[1], vectors[2]);
    }

    #[tokio::test]
    async fn empty_batches_succeed_but_empty_documents_and_queries_are_rejected() {
        let backend = DeterministicBackend::new();
        let embedder = Embedder::new().unwrap();
        assert!(backend.embed_documents(&[]).unwrap().is_empty());
        assert!(embedder.embed_documents(&[]).unwrap().is_empty());
        for blank in ["", " \t\n"] {
            assert_eq!(
                backend.embed_documents(&[blank.into()]),
                Err(EmbeddingError::EmptyDocument)
            );
            assert_eq!(
                embedder.embed_documents(&[blank.into()]),
                Err(EmbeddingError::EmptyDocument)
            );
            assert_eq!(backend.embed_query(blank), Err(EmbeddingError::EmptyQuery));
            assert_eq!(
                embedder.embed_query_cached(blank).await,
                Err(EmbeddingError::EmptyQuery)
            );
        }
    }

    #[test]
    fn default_embedder_preserves_the_deterministic_backend_identity() {
        let backend = DeterministicBackend::new();
        assert_eq!(backend.model_id(), "deterministic-hash");
        assert_eq!(backend.model_revision(), "deterministic-v2");
        assert_eq!(backend.dimensions(), 384);
        assert!(backend.normalized());
        let embedder = Embedder::default();
        let metadata = embedder.model_metadata();
        assert_eq!(metadata.model_id, backend.model_id());
        assert_eq!(metadata.model_revision, backend.model_revision());
        assert_eq!(metadata.dimensions, backend.dimensions());
        assert_eq!(metadata.normalized, backend.normalized());
    }

    #[test]
    fn skill_document_renders_all_sections_and_omits_empty_ones() {
        assert_eq!(
            SkillDocument::new("Just a description".into()).render(),
            "Just a description"
        );
        let document = SkillDocument::new("Parse JSON safely".into())
            .with_exports(vec![
                ("parseJson".into(), "(text: string): unknown | null".into()),
                ("validJson".into(), "(text: string): boolean".into()),
            ])
            .with_tags(vec!["json".into(), "parsing".into(), "utility".into()])
            .with_identifiers(vec!["parse_safe".into(), "json_parser_v2".into()]);
        assert_eq!(
            document.render(),
            concat!(
                "Parse JSON safely\n",
                "Exports: parseJson(text: string): unknown | null; validJson(text: string): boolean\n",
                "Tags: json, parsing, utility\n",
                "Identifiers: json_parser_v2, parse_safe"
            )
        );
    }

    #[test]
    fn skill_document_sorts_and_deduplicates_before_limiting_identifiers() {
        let identifiers = (0..20)
            .rev()
            .flat_map(|index| [format!("id_{index:02}"), format!("id_{index:02}")])
            .collect();
        let document = SkillDocument::new("Test".into()).with_identifiers(identifiers);
        assert_eq!(
            document.render(),
            concat!(
                "Test\nIdentifiers: id_00, id_01, id_02, id_03, id_04, ",
                "id_05, id_06, id_07, id_08, id_09"
            )
        );
    }

    #[tokio::test]
    async fn test_cache_normalized_queries() {
        let embedder = Embedder::new().unwrap();

        let query1 = "hello world";
        let query2 = "hello world  "; // Trailing spaces
        let query3 = "  hello world"; // Leading spaces

        embedder.embed_query_cached(query1).await.unwrap();
        let stats1 = embedder.cache_stats().await;
        assert_eq!(stats1.entries, 1);
        assert_eq!(stats1.hits, 0);

        embedder.embed_query_cached(query2).await.unwrap();
        let stats2 = embedder.cache_stats().await;
        // Trailing spaces are trimmed, so should be same cache entry
        assert_eq!(stats2.entries, 1);
        assert_eq!(stats2.hits, 1);

        embedder.embed_query_cached(query3).await.unwrap();
        let stats3 = embedder.cache_stats().await;
        // Leading spaces are also trimmed
        assert_eq!(stats3.entries, 1);
        assert_eq!(stats3.hits, 2);
    }

    #[test]
    fn test_error_types_display() {
        use crate::extras::js::skills::embed::EmbeddingError;

        assert_eq!(
            format!("{}", EmbeddingError::EmptyDocument),
            "empty document provided"
        );
        assert_eq!(
            format!("{}", EmbeddingError::EmptyQuery),
            "empty query provided"
        );
        assert_eq!(
            format!("{}", EmbeddingError::NonFiniteValue),
            "embedding contains non-finite value"
        );
        assert_eq!(
            format!(
                "{}",
                EmbeddingError::DimensionMismatch {
                    expected: 384,
                    actual: 256
                }
            ),
            "dimension mismatch: expected 384, got 256"
        );
        assert_eq!(
            format!("{}", EmbeddingError::Cancelled),
            "embedding inference was cancelled"
        );
        assert_eq!(
            format!("{}", EmbeddingError::WorkerSaturated),
            "embedding worker exhausted: too many concurrent requests"
        );
        assert_eq!(
            format!("{}", EmbeddingError::WorkerPanic),
            "embedding worker panicked"
        );
    }

    #[tokio::test]
    async fn test_multiple_embedders_independent_caches() {
        let embedder1 = Embedder::new().unwrap();
        let embedder2 = Embedder::new().unwrap();

        embedder1.embed_query_cached("query").await.unwrap();
        let stats1 = embedder1.cache_stats().await;
        let stats2 = embedder2.cache_stats().await;

        assert_eq!(stats1.entries, 1);
        assert_eq!(
            stats2.entries, 0,
            "different embedders should have separate caches"
        );
    }
}
