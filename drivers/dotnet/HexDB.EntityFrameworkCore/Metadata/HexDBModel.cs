// How entity types map to HexDB: each entity type is a tessellation (named
// after its DbSet, or ToTessellation; a hierarchy shares its root type's, with
// a discriminator field naming each document's type), each property a field of the document
// (named after the property, or ToJsonProperty), and the key is the
// document's `id`, a ULID. String keys get a new ULID when an entity is added;
// Guid keys are stored as the ULID with the same 128 bits.

using System.Text.RegularExpressions;
using HexDB.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Metadata.Builders;
using Microsoft.EntityFrameworkCore.Metadata.Conventions;
using Microsoft.EntityFrameworkCore.Metadata.Conventions.Infrastructure;

namespace HexDB.EntityFrameworkCore.Metadata
{
    /// <summary>Annotation names used by the HexDB provider.</summary>
    public static class HexDBAnnotationNames
    {
        public const string Tessellation = "HexDB:Tessellation";
        public const string JsonPropertyName = "HexDB:JsonPropertyName";
    }

    /// <summary>Adds the provider's conventions to EF Core's.</summary>
    public class HexDBConventionSetBuilder : ProviderConventionSetBuilder
    {
        public HexDBConventionSetBuilder(ProviderConventionSetBuilderDependencies dependencies) : base(dependencies) { }

        public override ConventionSet CreateConventionSet()
        {
            var set = base.CreateConventionSet();
            set.ModelFinalizingConventions.Add(new HexDBModelConvention(Dependencies));
            return set;
        }
    }

    /// <summary>Tessellation names from DbSet properties, generated ULID keys, Guid keys as ULIDs.</summary>
    public class HexDBModelConvention : IModelFinalizingConvention
    {
        private readonly ProviderConventionSetBuilderDependencies _dependencies;

        public HexDBModelConvention(ProviderConventionSetBuilderDependencies dependencies) => _dependencies = dependencies;

        public void ProcessModelFinalizing(IConventionModelBuilder modelBuilder, IConventionContext<IConventionModelBuilder> context)
        {
            var sets = _dependencies.SetFinder.FindSets(_dependencies.ContextType);
            foreach (var entityType in modelBuilder.Metadata.GetEntityTypes())
            {
                // Owned types live inside their owner's document.
                if (entityType.IsOwned()) continue;
                // Derived types are stored with their root type (one tessellation per hierarchy).
                if (entityType.BaseType is null && entityType.FindAnnotation(HexDBAnnotationNames.Tessellation) is null)
                {
                    var set = sets.Where(s => s.Type == entityType.ClrType).Select(s => s.Name).FirstOrDefault();
                    entityType.Builder.HasAnnotation(HexDBAnnotationNames.Tessellation, set ?? entityType.ClrType.Name);
                }
                var key = entityType.FindPrimaryKey();
                if (key is { Properties.Count: 1 })
                {
                    var property = key.Properties[0];
                    if (property.ClrType == typeof(string))
                    {
                        property.Builder.ValueGenerated(ValueGenerated.OnAdd);
                    }
                    else if (property.ClrType == typeof(Guid) && property.GetValueConverter() is null)
                    {
                        property.Builder.HasConversion(GuidUlidConverter.Instance);
                    }
                }
            }
        }
    }

    /// <summary>Rejects models the provider can't store, with the reason.</summary>
    public class HexDBModelValidator : ModelValidator
    {
        private static readonly Regex TessellationName = new("^[A-Za-z0-9-][A-Za-z0-9_-]{0,63}$");

        public HexDBModelValidator(ModelValidatorDependencies dependencies) : base(dependencies) { }

        public override void Validate(IModel model, IDiagnosticsLogger<DbLoggerCategory.Model.Validation> logger)
        {
            base.Validate(model, logger);
            foreach (var entityType in model.GetEntityTypes())
            {
                var name = entityType.DisplayName();
                // Owned types are stored in their owner's document; their keys come from it.
                if (entityType.IsOwned()) continue;
                if (entityType.BaseType is not null && entityType.FindAnnotation(HexDBAnnotationNames.Tessellation) is { Value: string own } && own != entityType.GetRootType().GetTessellation())
                    throw new InvalidOperationException($"'{name}' derives from '{entityType.GetRootType().DisplayName()}', so it's stored in that type's tessellation ('{entityType.GetRootType().GetTessellation()}'); call ToTessellation on the root type only.");
                if ((entityType.BaseType is not null || entityType.GetDirectlyDerivedTypes().Any()) && entityType.FindDiscriminatorProperty() is null)
                    throw new InvalidOperationException($"'{name}' is part of an inheritance hierarchy without a discriminator; HexDB stores a hierarchy in one tessellation and needs one to tell the types apart.");
                if (entityType.GetDeclaredSkipNavigations().FirstOrDefault() is { } skip)
                    throw new InvalidOperationException($"'{name}.{skip.Name}' is a many-to-many navigation; the HexDB provider doesn't support those. Map the join entity (with a navigation to each side) instead.");
                foreach (var foreignKey in entityType.GetDeclaredForeignKeys().Where(f => !f.IsOwnership))
                {
                    if (foreignKey.Properties.Count != 1)
                        throw new InvalidOperationException($"The relationship from '{name}' to '{foreignKey.PrincipalEntityType.DisplayName()}' has a composite foreign key; HexDB relationships use one foreign key property.");
                }
                var key = entityType.FindPrimaryKey()
                    ?? throw new InvalidOperationException($"'{name}' has no key. HexDB documents have an id: give the entity a string or Guid key.");
                if (key.Properties.Count != 1 || (key.Properties[0].ClrType != typeof(string) && key.Properties[0].ClrType != typeof(Guid)))
                    throw new InvalidOperationException($"'{name}' must have a single string or Guid key; HexDB ids are ULIDs (string keys get a new one when an entity is added).");
                var tessellation = entityType.GetTessellation();
                if (!TessellationName.IsMatch(tessellation))
                    throw new InvalidOperationException($"'{tessellation}' (for '{name}') isn't a valid tessellation name: 1-64 letters, digits, '_' or '-', not starting with '_'.");
            }
        }
    }
}

namespace Microsoft.EntityFrameworkCore
{
    using HexDB.EntityFrameworkCore.Metadata;

    /// <summary>HexDB-specific model configuration.</summary>
    public static class HexDBModelBuilderExtensions
    {
        /// <summary>Store this entity type in the named tessellation (default: the DbSet's name).</summary>
        public static EntityTypeBuilder<TEntity> ToTessellation<TEntity>(this EntityTypeBuilder<TEntity> builder, string name) where TEntity : class
        {
            builder.Metadata.SetAnnotation(HexDBAnnotationNames.Tessellation, name);
            return builder;
        }

        /// <summary>Store this property under another name in the document (default: the property's name).</summary>
        public static PropertyBuilder<TProperty> ToJsonProperty<TProperty>(this PropertyBuilder<TProperty> builder, string name)
        {
            builder.Metadata.SetAnnotation(HexDBAnnotationNames.JsonPropertyName, name);
            return builder;
        }

        /// <summary>The tessellation an entity type is stored in (its root type's, in a hierarchy).</summary>
        public static string GetTessellation(this IReadOnlyEntityType entityType)
        {
            var root = entityType.GetRootType();
            return root.FindAnnotation(HexDBAnnotationNames.Tessellation)?.Value as string ?? root.ClrType.Name;
        }

        /// <summary>Store an owned navigation under another name in the document (default: the navigation's name).</summary>
        public static OwnedNavigationBuilder<TOwner, TDependent> ToJsonProperty<TOwner, TDependent>(this OwnedNavigationBuilder<TOwner, TDependent> builder, string name)
            where TOwner : class where TDependent : class
        {
            builder.Metadata.PrincipalToDependent!.SetAnnotation(HexDBAnnotationNames.JsonPropertyName, name);
            return builder;
        }

        /// <summary>The document field an owned navigation is stored in.</summary>
        public static string GetJsonPropertyName(this IReadOnlyNavigationBase navigation) =>
            navigation.FindAnnotation(HexDBAnnotationNames.JsonPropertyName)?.Value as string ?? navigation.Name;

        /// <summary>The document field a property is stored in (`id` for the key).</summary>
        public static string GetJsonPropertyName(this IReadOnlyProperty property) =>
            property.IsPrimaryKey() ? "id" : property.FindAnnotation(HexDBAnnotationNames.JsonPropertyName)?.Value as string ?? property.Name;
    }
}
